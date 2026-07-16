// ── Agent 主循环 ──
//
// Agent 通过 mpsc channel 输出流式事件，与前端（Dioxus Signal / 未来 ACP）解耦。

use futures::StreamExt;
use tokio::sync::mpsc;

use super::hook::{AgentEvent, HookRegister};
use super::{AgentMode, ToolContext, ToolResult};
use crate::tools::Registry;
use crate::ui::state::ToolCallRecord;
use llm::{Chunk, Message, Provider, Request, Role as LlmRole, ToolCall};

// ── StreamEvent ──────────────────────────────────────────────────────────────

/// Agent 运行时通过 mpsc 发出的流式事件。
///
/// 复用 llm::Chunk 表示 LLM 流式输出，附加工具执行生命周期和循环状态。
#[derive(Debug, Clone)]
pub enum StreamEvent {
    /// LLM 流式事件（Text / Reasoning / ToolCallStart/Delta/Complete / Usage）
    Chunk(Chunk),
    /// 工具开始执行
    ToolExecuting {
        tool_name: String,
        args: serde_json::Value,
    },
    /// 工具执行完毕
    ToolExecuted {
        tool_name: String,
        result: Result<ToolResult, String>,
    },
    /// 循环正常结束
    Done,
    /// 错误
    Error(String),
}

// ── AgentOutput ──────────────────────────────────────────────────────────────

/// Agent 运行完毕后的最终产物（供持久化用）。
pub struct AgentOutput {
    pub output: String,
    pub reasoning: String,
    pub tool_records: Vec<ToolCallRecord>,
}

// ── Agent ────────────────────────────────────────────────────────────────────

use std::path::PathBuf;
use super::Agent;

impl Agent {
    /// 创建 Agent，获得 provider 和 registry 的所有权。
    pub fn new(
        provider: Box<dyn Provider>,
        registry: Registry,
        hook_register: HookRegister,
        plan_mode: bool,
        agent_mode: AgentMode,
        project_path: PathBuf,
    ) -> Self {
        Self {
            provider,
            registry,
            hook_register,
            plan_mode,
            agent_mode,
            project_path,
        }
    }

    /// 运行 agent 循环。
    ///
    /// - `user_input` — 用户本次输入
    /// - `history` — 历史消息（不含本次输入）
    /// - `tx` — 事件发送端，接收者收到 `StreamEvent::Done` 表示循环结束
    ///
    /// 返回最终产物 `AgentOutput`。
    pub async fn run(
        &self,
        user_input: String,
        history: &[Message],
        tx: mpsc::UnboundedSender<StreamEvent>,
    ) -> AgentOutput {
        // ── 1. 分发 Hook 事件 ──
        self.hook_register
            .emit(&AgentEvent::UserPromptSubmit {
                prompt: user_input.clone(),
            });

        // ── 2. 构建 LLM 消息历史 ──
        let mut req = Request {
            messages: history.to_vec(),
            tools: self.registry.schemas(),
            temperature: 0.7,
            max_tokens: 16384,
        };
        // 追加用户新消息
        req.messages.push(Message {
            role: LlmRole::User,
            content: Some(user_input),
            ..Default::default()
        });

        let mut final_output = String::new();
        let mut final_reasoning = String::new();

        // ── 3. Agent 循环 ──
        loop {
            let mut have_tool_calls = false;

            // ── 3a. LLM 流式调用 ──
            let mut stream = match self.provider.stream(&req).await {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("Stream error: {e}");
                    let _ = tx.send(StreamEvent::Error(msg.clone()));
                    final_output = msg;
                    break;
                }
            };

            tokio::pin!(stream);

            let mut output = String::new();
            let mut reasoning = String::new();
            let mut pending_tool_calls: Vec<ToolCall> = Vec::new();

            while let Some(result) = stream.next().await {
                match result {
                    Ok(chunk @ Chunk::Text(_)) => {
                        if let Chunk::Text(t) = &chunk {
                            output.push_str(t);
                        }
                        let _ = tx.send(StreamEvent::Chunk(chunk));
                    }
                    Ok(chunk @ Chunk::Reasoning { .. }) => {
                        if let Chunk::Reasoning { text, .. } = &chunk {
                            reasoning.push_str(text);
                        }
                        let _ = tx.send(StreamEvent::Chunk(chunk));
                    }
                    Ok(chunk @ Chunk::ToolCallComplete(_)) => {
                        if let Chunk::ToolCallComplete(tool) = &chunk {
                            have_tool_calls = true;
                            pending_tool_calls.push(tool.clone());
                        }
                        let _ = tx.send(StreamEvent::Chunk(chunk));
                    }
                    Ok(chunk @ Chunk::ToolCallStart { .. })
                    | Ok(chunk @ Chunk::ToolCallDelta { .. })
                    | Ok(chunk @ Chunk::Usage(_)) => {
                        let _ = tx.send(StreamEvent::Chunk(chunk));
                    }
                    Err(e) => {
                        let msg = format!("Stream error: {e}");
                        let _ = tx.send(StreamEvent::Error(msg.clone()));
                        output.push_str(&msg);
                        break;
                    }
                }
            }

            // ── 3b. Assistant 消息推入请求历史 ──
            {
                let mut msg = Message {
                    role: LlmRole::Assistant,
                    content: if output.is_empty() {
                        None
                    } else {
                        Some(output.clone())
                    },
                    ..Default::default()
                };
                if !pending_tool_calls.is_empty() {
                    msg.tool_calls = pending_tool_calls.clone();
                }
                req.messages.push(msg);
            }

            // ── 3c. 执行工具调用 ──
            for tool_call in &pending_tool_calls {
                let tool_name = tool_call.name.clone();

                // 分发 PreToolUse Hook
                self.hook_register.emit(&AgentEvent::PreToolUse {
                    tool_name: tool_name.clone(),
                    args: serde_json::from_str(&tool_call.arguments).unwrap_or_default(),
                });

                if let Some(tool) = self.registry.get(&tool_name) {
                    let args: serde_json::Value =
                        serde_json::from_str(&tool_call.arguments).unwrap_or_default();

                    // 通知前端：工具开始执行
                    let _ = tx.send(StreamEvent::ToolExecuting {
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                    });

                    // 执行工具
                    let ctx = ToolContext {
                        call_id: tool_call.id.clone(),
                        plan_mode: self.plan_mode,
                        agent_mode: self.agent_mode,
                        progress: None,
                    };
                    let result = tool.checked_execute(&ctx, &args).await;

                    // 通知前端：工具执行完毕
                    let _ = tx.send(StreamEvent::ToolExecuted {
                        tool_name: tool_name.clone(),
                        result: result.clone(),
                    });

                    // 分发 PostToolUse Hook
                    self.hook_register.emit(&AgentEvent::PostToolUse {
                        tool_name: tool_name.clone(),
                        result: result.clone(),
                    });

                    // 工具结果推入请求历史
                    req.messages.push(Message {
                        role: LlmRole::Tool,
                        content: Some(match &result {
                            Ok(tr) => tr.output.clone(),
                            Err(e) => format!("error: {e}"),
                        }),
                        tool_call_id: Some(tool_call.id.clone()),
                        name: Some(tool_name),
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

        // ── 4. 结束 ──
        let _ = tx.send(StreamEvent::Done);

        self.hook_register
            .emit(&AgentEvent::Stop {
                reason: "completed".into(),
            });

        AgentOutput {
            output: final_output,
            reasoning: final_reasoning,
            tool_records: Vec::new(), // bridge 层会从 channel 事件中累积
        }
    }
}
