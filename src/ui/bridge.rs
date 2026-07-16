// Agent 循环 → Dioxus signals 桥接层
//
// 初始化 Agent 结构体，通过 mpsc channel 消费流式事件并更新 UI Signal。

use dioxus::prelude::*;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, oneshot};

use crate::agent::hook::HookRegister;
use crate::agent::main_agent::{AgentOutput, StreamEvent};
use crate::agent::{Agent, AgentMode, ActionMode};
use crate::tools::Registry;
use crate::ui::state::*;
use llm::{Message, OpenAiProvider, Provider, Role as LlmRole};

/// 向 segments 追加片段，连续同类文本自动合并
fn push_segment(segments: &mut Vec<StreamSegment>, new: StreamSegment) {
    // 空列表 → 直接 push
    let Some(last) = segments.last_mut() else {
        segments.push(new);
        return;
    };

    // 类型不同 → push
    if std::mem::discriminant(last) != std::mem::discriminant(&new) {
        segments.push(new);
        return;
    }

    // 类型相同 → Text 和 Reasoning 合并，ToolCall 不会走到这里（ToolCall 自己就是 _ => true）
    match (last, &new) {
        (StreamSegment::Text(t), StreamSegment::Text(nt)) => t.push_str(&nt),
        (StreamSegment::Reasoning(r), StreamSegment::Reasoning(nr)) => r.push_str(&nr),
        _ => segments.push(new),  
    }
}

/// 在后台异步任务中运行完整的 agent 循环。
pub async fn run_agent_loop(
    user_input: String,
    config: AppConfig,
    action_mode: ActionMode,
    agent_mode: AgentMode,
    approval_responder: Arc<Mutex<Option<oneshot::Sender<bool>>>>,
    mut pending_approval: Signal<Option<PendingApproval>>,
    mut messages: Signal<Vec<ChatMessage>>,
    mut streaming_segments: Signal<Vec<StreamSegment>>,
    mut is_streaming: Signal<bool>,
    mut active_tool_calls: Signal<Vec<ToolCallRecord>>,
    mut projects: Signal<Vec<Project>>,
    active_project_id: Signal<Option<String>>,
    active_conversation_id: Signal<String>,
    mut streaming_project_id: Signal<Option<String>>,
) {
    is_streaming.set(true);
    streaming_segments.set(Vec::new());
    active_tool_calls.set(Vec::new());

    // ── 1. 构建 LLM provider ──
    let provider = match OpenAiProvider::new(
        config.model.clone(),
        config.base_url.clone(),
        config.model.clone(),
        config.api_key.clone(),
        None,
        false,
        None,
    ) {
        Ok(p) => Box::new(p) as Box<dyn Provider>,
        Err(e) => {
            streaming_segments.write().push(StreamSegment::Text(format!("Provider error: {e}")));
            is_streaming.set(false);
            streaming_project_id.set(None);
            return;
        }
    };

    // ── 2. 确定项目路径 ──
    let project_path = {
        let pid = active_project_id.read().clone();
        let projs = projects.read();
        pid.as_ref()
            .and_then(|id| projs.iter().find(|p| p.id == *id).map(|p| p.path.clone()))
            .unwrap_or_else(|| ".".to_string())
    };
    let project_dir = std::path::Path::new(&project_path);

    // ── 3. 构建工具注册表 ──
    let registry = Registry::new();
    crate::tools::register_builtins(&registry, project_dir);

    // ── 捕获原始目标对话（防止用户中途切换导致回复跑错地方）──
    let origin_proj_id = active_project_id.read().clone();
    let origin_conv_id = active_conversation_id.read().clone();

    // ── 4. 构建 Agent ──
    let hook_register = HookRegister::new();
    let agent = Agent::new(provider, registry, hook_register, action_mode, agent_mode, project_path.into());

    // ── 5. 构建 LLM 消息历史 ──
    let mut history: Vec<Message> = vec![Message {
        role: LlmRole::System,
        content: Some("You are a helpful assistant.".into()),
        ..Default::default()
    }];

    {
        let msgs = messages.read();
        for m in msgs.iter() {
            history.push(Message {
                role: match m.role {
                    Role::User => LlmRole::User,
                    Role::Assistant => LlmRole::Assistant,
                    Role::System => LlmRole::System,
                },
                content: Some(m.content.clone()),
                ..Default::default()
            });
        }
    }

    // ── 5. 创建 mpsc channel，启动 Agent ──
    let (tx, mut rx) = mpsc::unbounded_channel::<StreamEvent>();

    let agent_handle = tokio::spawn(async move {
        agent.run(user_input, &history, tx).await
    });

    // ── 6. 消费流式事件，更新 UI（按 LLM 返回顺序推送片段）──
    // 本地累积（无论是否切换对话都记录，确保保存时完整）
    // 使用 push_segment 合并连续同类片段，保证保存和显示的 segments 结构一致
    let mut final_output = String::new();
    let mut final_reasoning = String::new();
    let mut all_tool_records: Vec<ToolCallRecord> = Vec::new();
    let mut local_segments: Vec<StreamSegment> = Vec::new();
    let mut was_on_original = true;

    while let Some(event) = rx.recv().await {
        let on_original = active_conversation_id.read().clone() == origin_conv_id;

        // 检测到切回原对话：用完整累积内容替换显示信号（确保 toolcall index 对齐）
        if on_original && !was_on_original {
            active_tool_calls.set(all_tool_records.clone());
            streaming_segments.set(local_segments.clone());
        }

        match event {
            StreamEvent::Chunk(llm::Chunk::Text(t)) => {
                final_output.push_str(&t);
                push_segment(&mut local_segments, StreamSegment::Text(t.clone()));
                if on_original {
                    push_segment(&mut streaming_segments.write(), StreamSegment::Text(t));
                }
            }
            StreamEvent::Chunk(llm::Chunk::Reasoning { text, .. }) => {
                final_reasoning.push_str(&text);
                push_segment(&mut local_segments, StreamSegment::Reasoning(text.clone()));
                if on_original {
                    push_segment(&mut streaming_segments.write(), StreamSegment::Reasoning(text));
                }
            }
            StreamEvent::Chunk(_) => {
                // ToolCallStart/Delta/Complete/Usage 暂不需要 UI 更新
            }
            StreamEvent::ToolExecuting { tool_name, args } => {
                all_tool_records.push(ToolCallRecord {
                    tool_name: tool_name.clone(),
                    args: args.clone(),
                    result: None,
                    status: ToolCallStatus::Running,
                    approval_reason: None,
                });
                local_segments.push(StreamSegment::ToolCall);
                if on_original {
                    push_segment(&mut streaming_segments.write(), StreamSegment::ToolCall);
                    active_tool_calls.write().push(ToolCallRecord {
                        tool_name,
                        args,
                        result: None,
                        status: ToolCallStatus::Running,
                        approval_reason: None,
                    });
                }
            }
            StreamEvent::ToolExecuted { tool_name, result } => {
                // 更新本地累积中的记录
                if let Some(record) = all_tool_records.iter_mut().rev().find(|tc| {
                    tc.tool_name == tool_name && (tc.status == ToolCallStatus::Running || matches!(tc.status, ToolCallStatus::AwaitingApproval{..}))
                }) {
                    record.result = Some(match &result {
                        Ok(tr) => tr.output.clone(),
                        Err(e) => e.clone(),
                    });
                    record.status = match &result {
                        Ok(_) => ToolCallStatus::Success,
                        Err(_) => ToolCallStatus::Failed("failed".into()),
                    };
                }
                if on_original {
                    let mut calls = active_tool_calls.write();
                    if let Some(record) = calls.iter_mut().rev().find(|tc| {
                        tc.tool_name == tool_name && (tc.status == ToolCallStatus::Running || matches!(tc.status, ToolCallStatus::AwaitingApproval{..}))
                    }) {
                        record.result = Some(match &result {
                            Ok(tr) => tr.output.clone(),
                            Err(e) => e.clone(),
                        });
                        record.status = match &result {
                            Ok(_) => ToolCallStatus::Success,
                            Err(_) => ToolCallStatus::Failed("failed".into()),
                        };
                    }
                    // 清除审批状态
                    pending_approval.set(None);
                }
            }
            StreamEvent::ToolNeedsApproval { tool_name, args, reason, approval_tx } => {
                // 存储 oneshot sender 供 UI 回调使用
                *approval_responder.lock().unwrap() = Some(approval_tx);
                // 更新工具调用状态
                if let Some(record) = all_tool_records.iter_mut().rev().find(|tc| {
                    tc.tool_name == tool_name && tc.status == ToolCallStatus::Running
                }) {
                    record.status = ToolCallStatus::AwaitingApproval { reason: reason.clone() };
                    record.approval_reason = Some(reason.clone());
                }
                local_segments.push(StreamSegment::ToolCall);
                if on_original {
                    // 更新 active_tool_calls 中对应记录的状态
                    let mut calls = active_tool_calls.write();
                    if let Some(record) = calls.iter_mut().rev().find(|tc| {
                        tc.tool_name == tool_name && tc.status == ToolCallStatus::Running
                    }) {
                        record.status = ToolCallStatus::AwaitingApproval { reason: reason.clone() };
                        record.approval_reason = Some(reason.clone());
                    }
                    // 通知 UI 有审批请求
                    pending_approval.set(Some(PendingApproval {
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                        reason: reason.clone(),
                    }));
                }
            }
            StreamEvent::Done => break,
            StreamEvent::Error(msg) => {
                final_output = msg;
                break;
            }
        }
        was_on_original = on_original;
    }

    // ── 7. 等待 Agent 完成，获取最终产物 ──
    let on_original_before_wait = active_conversation_id.read().clone() == origin_conv_id;
    let _agent_output: AgentOutput = match agent_handle.await {
        Ok(output) => output,
        Err(e) => {
            if on_original_before_wait {
                streaming_segments.write().push(StreamSegment::Text(format!("Agent panicked: {e}")));
                streaming_segments.set(Vec::new());
                active_tool_calls.set(Vec::new());
            }
            is_streaming.set(false);
            streaming_project_id.set(None);
            return;
        }
    };

    // ── 8. 将最终响应写入消息列表和持久化存储 ──
    let is_frontend_on_this_conv = active_conversation_id.read().clone() == origin_conv_id;
    if !final_output.is_empty() {
        let tool_records = std::mem::take(&mut all_tool_records);
        let complete_segments = std::mem::take(&mut local_segments);
        active_tool_calls.set(Vec::new());

        if is_frontend_on_this_conv {
            messages.write().push(ChatMessage {
                role: Role::Assistant,
                content: final_output.clone(),
                timestamp: chrono::Local::now(),
                tool_calls: tool_records.clone(),
                reasoning: final_reasoning.clone(),
                segments: streaming_segments.read().clone(),
            });
        }

        // 保存到原始对话（使用完整的有序 segments，确保 tool calls 位置正确）
        if let (Some(ref pid), cid) = (origin_proj_id, origin_conv_id) {
            if !cid.is_empty() {
                let mut projs = projects.write();
                if let Some(proj) = projs.iter_mut().find(|p| p.id == *pid) {
                    if let Some(conv) = proj.conversations.iter_mut().find(|c| c.id == cid) {
                        conv.messages.push(ChatMessage {
                            role: Role::Assistant,
                            content: final_output,
                            timestamp: chrono::Local::now(),
                            tool_calls: tool_records,
                            reasoning: final_reasoning,
                            segments: complete_segments,
                        });
                        conv.updated_at = chrono::Local::now();
                    }
                    proj.last_activity_at = Some(chrono::Local::now());
                }
                crate::ui::store::save_projects_quiet(&projs);
            }
        }
    }

    // 清理显示状态：仅当前端仍在原对话时才清空
    if is_frontend_on_this_conv {
        streaming_segments.set(Vec::new());
        active_tool_calls.set(Vec::new());
    }
    is_streaming.set(false);
    streaming_project_id.set(None);
}
