// Agent 循环 → Dioxus signals 桥接层
//
// Signal<T> 是 Copy 的，可直接移入 spawn 的 async 闭包中。
// agent 循环在后台 tokio 任务中运行，通过 signal.set() 实时更新 UI。

use dioxus::prelude::*;

use crate::agent::AgentMode;
use crate::tools::Registry;
use crate::ui::state::*;
use llm::{Chunk, Message, OpenAiProvider, Provider, Request, Role as LlmRole, ToolCall};

/// 在后台异步任务中运行完整的 agent 循环。
///
/// # 参数
/// - `user_input`: 用户输入文本
/// - `config`: 应用配置（模型、API key 等）
/// - `messages`: 当前对话消息列表 signal
/// - `streaming_content`: 当前流式输出内容 signal
/// - `streaming_reasoning`: 当前流式推理内容 signal
/// - `is_streaming`: 是否正在运行 signal
/// - `active_tool_calls`: 当前活跃工具调用 signal
pub async fn run_agent_loop(
    user_input: String,
    config: AppConfig,
    mut messages: Signal<Vec<ChatMessage>>,
    mut streaming_content: Signal<String>,
    mut streaming_reasoning: Signal<String>,
    mut is_streaming: Signal<bool>,
    mut active_tool_calls: Signal<Vec<ToolCallRecord>>,
    mut projects: Signal<Vec<Project>>,
    active_project_id: Signal<Option<String>>,
    active_conversation_id: Signal<String>,
) {
    is_streaming.set(true);
    streaming_content.set(String::new());
    streaming_reasoning.set(String::new());
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
        Ok(p) => p,
        Err(e) => {
            streaming_content.set(format!("Provider error: {e}"));
            is_streaming.set(false);
            return;
        }
    };

    // ── 2. 构建工具注册表 ──
    let registry = Registry::new();
    crate::tools::register_builtins(&registry);

    // ── 3. 构建 LLM 消息历史 ──
    let mut llm_messages: Vec<Message> = vec![Message {
        role: LlmRole::System,
        content: Some("You are a helpful assistant.".into()),
        ..Default::default()
    }];

    // 将历史消息转换为 LLM 格式
    {
        let msgs = messages.read();
        for m in msgs.iter() {
            llm_messages.push(Message {
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

    // 追加用户新消息
    llm_messages.push(Message {
        role: LlmRole::User,
        content: Some(user_input),
        ..Default::default()
    });

    let mut req = Request {
        messages: llm_messages,
        tools: registry.schemas(),
        temperature: config.temperature,
        max_tokens: config.max_tokens,
    };

    let mut final_output = String::new();
    let mut final_reasoning = String::new();

    // ── 4. Agent 循环 ──
    loop {
        let mut have_tool_calls = false;

        let mut stream = match provider.stream(&req).await {
            Ok(s) => s,
            Err(e) => {
                streaming_content.set(format!("Stream error: {e}"));
                break;
            }
        };

        use futures::StreamExt;
        tokio::pin!(stream);

        let mut output = String::new();
        let mut reasoning = String::new();
        let mut pending_tool_calls: Vec<ToolCall> = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(Chunk::Text(t)) => {
                    output.push_str(&t);
                    streaming_content.set(output.clone());
                }
                Ok(Chunk::Reasoning { text, .. }) => {
                    reasoning.push_str(&text);
                    streaming_reasoning.set(reasoning.clone());
                }
                Ok(Chunk::ToolCallComplete(tool)) => {
                    have_tool_calls = true;
                    pending_tool_calls.push(tool);
                }
                Ok(Chunk::Usage(_)) => {
                    // 忽略用量信息
                }
                Err(e) => {
                    streaming_content.set(format!("Error: {e}"));
                    break;
                }
                _ => {}
            }
        }

        // 将 assistant 消息（含 tool_calls）推入请求历史
        {
            let mut msg = Message {
                role: LlmRole::Assistant,
                content: if output.is_empty() { None } else { Some(output.clone()) },
                ..Default::default()
            };
            if !pending_tool_calls.is_empty() {
                msg.tool_calls = pending_tool_calls.clone();
            }
            req.messages.push(msg);
        }

        // 执行工具调用
        for tool in &pending_tool_calls {
            if let Some(t) = registry.get(&tool.name) {
                let ctx = crate::agent::ToolContext {
                    call_id: tool.id.clone(),
                    plan_mode: false,
                    agent_mode: AgentMode::Ask,
                    progress: None,
                };

                let args: serde_json::Value =
                    serde_json::from_str(&tool.arguments).unwrap_or_default();

                // 更新 UI：添加运行中的工具调用
                active_tool_calls.write().push(ToolCallRecord {
                    tool_name: tool.name.clone(),
                    args: args.clone(),
                    result: None,
                    status: ToolCallStatus::Running,
                });

                let result = t.checked_execute(&ctx, &args).await;

                // 更新 UI：工具执行结果
                {
                    let mut calls = active_tool_calls.write();
                    if let Some(record) = calls.iter_mut().rev().find(|tc| {
                        tc.tool_name == tool.name && tc.status == ToolCallStatus::Running
                    }) {
                        record.result = Some(match &result {
                            Ok(tr) => tr.output.clone(),
                            Err(e) => e.clone(),
                        });
                        record.status = match &result {
                            Ok(_) => ToolCallStatus::Success,
                            Err(e) => ToolCallStatus::Failed(e.clone()),
                        };
                    }
                }

                // 工具结果推入请求历史
                req.messages.push(Message {
                    role: LlmRole::Tool,
                    content: Some(match &result {
                        Ok(tr) => tr.output.clone(),
                        Err(e) => format!("error: {e}"),
                    }),
                    tool_call_id: Some(tool.id.clone()),
                    name: Some(tool.name.clone()),
                    ..Default::default()
                });
            }
        }

        final_output = output;
        final_reasoning.push_str(&reasoning);

        if !have_tool_calls {
            break;
        }
    }

    // ── 5. 循环结束，将最终响应写入消息列表和持久化存储 ──
    let final_content = final_output;
    if !final_content.is_empty() {
        messages.write().push(ChatMessage {
            role: Role::Assistant,
            content: final_content,
            timestamp: chrono::Local::now(),
            tool_calls: active_tool_calls.read().clone(),
            reasoning: final_reasoning,
        });

        // 同步写入项目对话存储
        let proj_id = active_project_id.read().clone();
        let conv_id = active_conversation_id.read().clone();
        if let (Some(ref pid), cid) = (proj_id, conv_id) {
            if !cid.is_empty() {
                let msgs = messages.read().clone();
                let mut projs = projects.write();
                if let Some(proj) = projs.iter_mut().find(|p| p.id == *pid) {
                    if let Some(conv) = proj.conversations.iter_mut().find(|c| c.id == cid) {
                        conv.messages = msgs;
                    }
                }
                crate::ui::store::save_projects_quiet(&projs);
            }
        }
    }

    streaming_content.set(String::new());
    streaming_reasoning.set(String::new());
    is_streaming.set(false);
}
