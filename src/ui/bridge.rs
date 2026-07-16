// Agent 循环 → Dioxus signals 桥接层
//
// 初始化 Agent 结构体，通过 mpsc channel 消费流式事件并更新 UI Signal。

use dioxus::prelude::*;
use tokio::sync::mpsc;

use crate::agent::hook::HookRegister;
use crate::agent::main_agent::{AgentOutput, StreamEvent};
use crate::agent::{Agent, AgentMode};
use crate::tools::Registry;
use crate::ui::state::*;
use llm::{Message, OpenAiProvider, Provider, Role as LlmRole};

/// 向 segments 追加片段，连续同类文本自动合并
fn push_segment(segments: &mut Vec<StreamSegment>, new: StreamSegment) {
    let should_push = match &new {
        StreamSegment::Text(t) => {
            if let Some(StreamSegment::Text(last)) = segments.last_mut() {
                last.push_str(t);
                false
            } else {
                true
            }
        }
        StreamSegment::Reasoning(r) => {
            if let Some(StreamSegment::Reasoning(last)) = segments.last_mut() {
                last.push_str(r);
                false
            } else {
                true
            }
        }
        _ => true,
    };
    if should_push {
        segments.push(new);
    }
}

/// 在后台异步任务中运行完整的 agent 循环。
pub async fn run_agent_loop(
    user_input: String,
    config: AppConfig,
    mut messages: Signal<Vec<ChatMessage>>,
    mut streaming_segments: Signal<Vec<StreamSegment>>,
    mut is_streaming: Signal<bool>,
    mut active_tool_calls: Signal<Vec<ToolCallRecord>>,
    mut projects: Signal<Vec<Project>>,
    active_project_id: Signal<Option<String>>,
    active_conversation_id: Signal<String>,
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

    // ── 4. 构建 Agent ──
    let hook_register = HookRegister::new();
    let agent = Agent::new(provider, registry, hook_register, false, AgentMode::Ask, project_path.into());

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
    let mut final_output = String::new();
    let mut final_reasoning = String::new();

    while let Some(event) = rx.recv().await {
        match event {
            StreamEvent::Chunk(llm::Chunk::Text(t)) => {
                final_output.push_str(&t);
                push_segment(&mut streaming_segments.write(), StreamSegment::Text(t));
            }
            StreamEvent::Chunk(llm::Chunk::Reasoning { text, .. }) => {
                final_reasoning.push_str(&text);
                push_segment(&mut streaming_segments.write(), StreamSegment::Reasoning(text));
            }
            StreamEvent::Chunk(_) => {
                // ToolCallStart/Delta/Complete/Usage 暂不需要 UI 更新
            }
            StreamEvent::ToolExecuting { tool_name, args } => {
                push_segment(&mut streaming_segments.write(), StreamSegment::ToolCall);
                active_tool_calls.write().push(ToolCallRecord {
                    tool_name,
                    args,
                    result: None,
                    status: ToolCallStatus::Running,
                });
            }
            StreamEvent::ToolExecuted { tool_name, result } => {
                let mut calls = active_tool_calls.write();
                if let Some(record) = calls.iter_mut().rev().find(|tc| {
                    tc.tool_name == tool_name && tc.status == ToolCallStatus::Running
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
            }
            StreamEvent::Done => break,
            StreamEvent::Error(msg) => {
                final_output = msg;
                break;
            }
        }
    }

    // ── 7. 等待 Agent 完成，获取最终产物 ──
    let _agent_output: AgentOutput = match agent_handle.await {
        Ok(output) => output,
        Err(e) => {
            streaming_segments.write().push(StreamSegment::Text(format!("Agent panicked: {e}")));
            streaming_segments.set(Vec::new());
            active_tool_calls.set(Vec::new());
            is_streaming.set(false);
            return;
        }
    };

    // ── 8. 将最终响应写入消息列表和持久化存储 ──
    if !final_output.is_empty() {
        let tool_records = active_tool_calls.read().clone();
        active_tool_calls.set(Vec::new());

        messages.write().push(ChatMessage {
            role: Role::Assistant,
            content: final_output,
            timestamp: chrono::Local::now(),
            tool_calls: tool_records,
            reasoning: final_reasoning,
            segments: streaming_segments.read().clone(),
        });

        let proj_id = active_project_id.read().clone();
        let conv_id = active_conversation_id.read().clone();
        if let (Some(ref pid), cid) = (proj_id, conv_id) {
            if !cid.is_empty() {
                let msgs = messages.read().clone();
                let mut projs = projects.write();
                if let Some(proj) = projs.iter_mut().find(|p| p.id == *pid) {
                    if let Some(conv) = proj.conversations.iter_mut().find(|c| c.id == cid) {
                        conv.messages = msgs;
                        conv.updated_at = chrono::Local::now();
                    }
                }
                crate::ui::store::save_projects_quiet(&projs);
            }
        }
    }

    // 无论是否有输出，清空临时状态
    streaming_segments.set(Vec::new());
    active_tool_calls.set(Vec::new());
    is_streaming.set(false);
}
