// ── Agent 主循环 ──
//
// Agent 自己管理消息历史 + 本地持久化。
// 流式数据通过 Arc 共享给 UI，不再使用 mpsc channel。

use futures::StreamExt;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio_util::sync::CancellationToken;

use super::hook::AgentEvent;
use super::ToolContext;
use crate::model::{ChatMessage, Role as ChatRole, StreamSegment, ToolCallRecord, ToolCallStatus, UiMessage};
use crate::permission::Decision;
use llm::{Chunk, Message, Request, Role as LlmRole};

// ── select! 辅助枚举 ────────────────────────────────────────────────────────

enum StreamOrCancel<T> {
    Chunk(T),
    Cancelled,
}

// ── Agent::accept_message ────────────────────────────────────────────

use super::Agent;

impl Agent {
    /// 创建内部流式状态，返回 UiMessage::Streaming 供 UI 显示。
    /// 必须先调用此方法，再调用 accept_message。
    pub fn create_streaming(&mut self) -> UiMessage {
        if self.streaming_handle.is_none()  {
            let state = crate::model::StreamingState {
                segments: Arc::new(Mutex::new(Vec::new())),
                tool_calls: Arc::new(Mutex::new(Vec::new())),
                version: Arc::new(AtomicU64::new(0)),
                approval_tx: Arc::new(Mutex::new(None)),
            };
            self.streaming_handle = Some(state);
        }        

        let streaming = UiMessage::Streaming {
            segments: self.streaming_handle.as_ref().unwrap().segments.clone(),
            tool_calls: self.streaming_handle.as_ref().unwrap().tool_calls.clone(),
            version: self.streaming_handle.as_ref().unwrap().version.clone(),
            approval_tx: self.streaming_handle.as_ref().unwrap().approval_tx.clone(),
        };
        
        streaming
    }

    /// 接受用户输入并运行 agent 循环。返回已完成的 UiMessage::Static。
    ///
    /// 调用前必须先调用 create_streaming() 初始化内部流式状态。
    pub async fn accept_message(
        &mut self,
        user_input: String,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<UiMessage> {

        let (segments_arc, tool_calls_arc, version_arc, approval_arc) =
            {
                self.create_streaming();
                let ss = self.streaming_handle.as_ref().unwrap_or_else(|| {
                    panic!("create_streaming() must be called first")
                });
                (
                    ss.segments.clone(),
                    ss.tool_calls.clone(),
                    ss.version.clone(),
                    ss.approval_tx.clone(),
                )
            };
        
        self.hook_register.emit(&AgentEvent::UserPromptSubmit { prompt: user_input.clone() });

        // ── 前缀注入 目前只有 Plan Mode ──
        let augmented_input = self.handler.prompt_before_user_message();
        if let Some(augmented_input) = augmented_input {
            self.push_message(Message { role: LlmRole::System, content: Some(augmented_input.to_string()), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;
        }
        // 用户输入
        self.push_message(Message { role: LlmRole::User, content: Some(user_input), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;

        
        let mut _final_output = String::new();
        let mut final_reasoning = String::new();
        let mut cancelled = false;
 
        
        if let Some(pre_prompt) = self.handler.prompt_pre_loop() {
            self.push_message(Message { role: LlmRole::System, content: Some(pre_prompt), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;
        }

        self.start_loop();
        
        loop {
            self.round_start();
            let mut have_tool_calls = false;

            // Agent 回合日志
            tracing::debug!(
                target: "agent",
                round = self.round.ok_or(anyhow::anyhow!("round must be set"))?,
                messages = self.messages.len(),
                tools = self.registry.schemas().len(),
                "agent round"
            );

            let req = Request {
                messages: self.messages.clone(),
                tools: self.registry.schemas(),
                temperature: self.temperature,
                max_tokens: 65536,
            };

            let stream = match self.provider.stream(&req).await {
                Ok(s) => s,
                Err(e) => {
                    let msg = format!("Stream error: {e}");
                    push_text(&segments_arc, &msg, &version_arc);
                    _final_output = msg;
                    break;
                }
            };

            tokio::pin!(stream);

            let mut output = String::new();
            let mut reasoning = String::new();
            

            loop {
                let result = tokio::select! {
                    _ = cancel_token.cancelled() => StreamOrCancel::Cancelled,
                    r = stream.next() => StreamOrCancel::Chunk(r),
                };

                match result {
                    StreamOrCancel::Cancelled => { cancelled = true; break; }
                    StreamOrCancel::Chunk(None) => break,
                    StreamOrCancel::Chunk(Some(Ok(Chunk::Text(t)))) => {
                        output.push_str(&t);
                        push_text(&segments_arc, &t, &version_arc);
                    }
                    StreamOrCancel::Chunk(Some(Ok(Chunk::Reasoning { text, .. }))) => {
                        reasoning.push_str(&text);
                        push_reasoning(&segments_arc, &text, &version_arc);
                    }
                    StreamOrCancel::Chunk(Some(Ok(Chunk::ToolCallComplete(tc)))) => {
                        have_tool_calls = true;
                        self.pending_tool_calls.push(tc.clone());
                    }
                    StreamOrCancel::Chunk(Some(Ok(_))) => {} // Start/Delta/Usage
                    StreamOrCancel::Chunk(Some(Err(e))) => {
                        let msg = format!("Stream error: {e}");
                        output.push_str(&msg);
                        push_text(&segments_arc, &msg, &version_arc);
                        break;
                    }
                }
            }

            if cancelled {
                let output_empty = output.is_empty();
                let reasoning_empty = reasoning.is_empty();
                _final_output = output.clone();
                final_reasoning.push_str(&reasoning);
                if !output_empty || !self.pending_tool_calls.is_empty() {
                    let msg = Message {
                        role: LlmRole::Assistant,
                        content: if output_empty { None } else { Some(output.clone()) },
                        reasoning_content: if reasoning_empty { None } else { Some(reasoning.clone()) },
                        tool_calls: if self.pending_tool_calls.is_empty() { Vec::new() } else { self.pending_tool_calls.clone() },
                        timestamp: Some(chrono::Utc::now()),
                        ..Default::default()
                    };
                    self.push_message(msg)?;
                }
                break;
            }


            // Assistant 消息入 self.messages
            {
                let mut msg = Message {
                    role: LlmRole::Assistant,
                    content: if output.is_empty() { None } else { Some(output.clone()) },
                    reasoning_content: if reasoning.is_empty() { None } else { Some(reasoning.clone()) },
                    timestamp: Some(chrono::Utc::now()),
                    ..Default::default()
                };
                if !self.pending_tool_calls.is_empty() {
                    msg.tool_calls = self.pending_tool_calls.clone();
                }
                self.push_message(msg)?;
            }

            // 执行工具调用
            for i in 0..self.pending_tool_calls.len() {
                let tool_call = &self.pending_tool_calls[i];
                if cancelled { break; }
                let tool_name = tool_call.name.clone();
                self.hook_register.emit(&AgentEvent::PreToolUse {
                    tool_name: tool_name.clone(),
                    args: serde_json::from_str(&tool_call.arguments).unwrap_or_default(),
                });

                if let Some(tool) = self.registry.get(&tool_name) {
                    let args: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or_default();

                    // push tool record + ToolCall marker
                    {
                        let mut tcs = tool_calls_arc.lock().expect("tool_calls_arc lock poisoned");
                        tcs.push(ToolCallRecord {
                            tool_name: tool_name.clone(), args: args.clone(),
                            result: None, status: ToolCallStatus::Running, approval_reason: None,
                        });
                    }
                    push_tool_marker(&segments_arc, &version_arc);

                    let ctx = ToolContext { call_id: tool_call.id.clone(), plan_mode: self.handler.action_mode(), handler: self.handler.clone(), progress: None, main_conversation_id: self.conversation_id.clone(), project_id: self.project_id.clone(), cancel_token: Some(cancel_token.clone()) };
                    let decision = tool.pre_check(&ctx, &args);
                    let is_denied = matches!(decision, Decision::Deny(_));
                    let result = match decision {
                        Decision::Allow => {
                            let exec = tool.execute(&ctx, &args);
                            tokio::pin!(exec);
                            tokio::select! {
                                _ = cancel_token.cancelled() => {
                                    cancelled = true;
                                    Err("cancelled by user".into())
                                }
                                r = &mut exec => r,
                            }
                        }
                        Decision::Ask => {
                            let reason = format!("{} needs approval", tool_name);
                            {
                                let mut tcs = tool_calls_arc.lock().expect("tool_calls_arc lock poisoned");
                                if let Some(rec) = tcs.iter_mut().rev().find(|tc| tc.tool_name == tool_name && tc.status == ToolCallStatus::Running) {
                                    rec.status = ToolCallStatus::AwaitingApproval { reason: reason.clone() };
                                    rec.approval_reason = Some(reason.clone());
                                }
                            }
                            push_tool_marker(&segments_arc, &version_arc);

                            let (atx, arx) = tokio::sync::oneshot::channel();
                            *approval_arc.lock().expect("approval_arc lock poisoned") = Some(atx);

                            let approval = tokio::select! {
                                _ = cancel_token.cancelled() => {
                                    cancelled = true;
                                    None
                                }
                                r = arx => r.ok(),
                            };

                            match approval {
                                Some(true) => {
                                    // 立即更新状态为 Running，让 UI 及时响应
                                    {
                                        let mut tcs = tool_calls_arc.lock().expect("tool_calls_arc lock poisoned");
                                        if let Some(rec) = tcs.iter_mut().rev().find(|tc| {
                                            tc.tool_name == tool_name && matches!(tc.status, ToolCallStatus::AwaitingApproval { .. })
                                        }) {
                                            rec.status = ToolCallStatus::Running;
                                        }
                                    }
                                    *approval_arc.lock().expect("approval_arc lock poisoned") = None;
                                    inc_version_atomic(&version_arc);
                                    let exec = tool.execute(&ctx, &args);
                                    tokio::pin!(exec);
                                    tokio::select! {
                                        _ = cancel_token.cancelled() => {
                                            cancelled = true;
                                            Err("cancelled by user".into())
                                        }
                                        r = &mut exec => r,
                                    }
                                }
                                Some(false) => Err(format!("denied by user: {reason}")),
                                None => {
                                    cancelled = true;
                                    Err("cancelled by user".into())
                                }
                            }
                        }
                        Decision::Deny(msg) => Err(msg),
                    };

                    // 更新 tool record 状态
                    {
                        let mut tcs = tool_calls_arc.lock().expect("tool_calls_arc lock poisoned");
                        if let Some(rec) = tcs.iter_mut().rev().find(|tc| {
                            tc.tool_name == tool_name
                                && (tc.status == ToolCallStatus::Running || matches!(tc.status, ToolCallStatus::AwaitingApproval { .. }))
                        }) {
                            rec.result = Some(match &result { Ok(tr) => tr.output.clone(), Err(e) => e.clone() });
                            rec.status = match &result {
                                Ok(_) => ToolCallStatus::Success,
                                Err(e) if is_denied || e.starts_with("denied by user:") || e == "approval channel closed" => {
                                    ToolCallStatus::Denied(e.clone())
                                }
                                Err(e) => ToolCallStatus::Failed(e.clone()),
                            };
                        }
                    }
                    *approval_arc.lock().expect("approval_arc lock poisoned") = None;
                    inc_version_atomic(&version_arc);

                    self.hook_register.emit(&AgentEvent::PostToolUse { tool_name: tool_name.clone(), result: result.clone() });
                    let tool_message = Message {
                        role: LlmRole::Tool,
                        content: Some(match &result { Ok(tr) => tr.output.clone(), Err(e) => format!("error: {e}") }),
                        tool_call_id: Some(tool_call.id.clone()), tool_name: Some(tool_name),
                        timestamp: Some(chrono::Utc::now()),
                        ..Default::default()
                    };
                    self.push_message(tool_message)?;
                }
            }
                
            if cancelled { break; }
            
            _final_output = output;
            final_reasoning.push_str(&reasoning);

            self.round_end();
            
            if !have_tool_calls { 
                if let Some(reason) = self.handler.can_finish() {
                    self.push_message(Message { role: LlmRole::System, content: Some(reason), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;
                    continue;
                }
                break; 
            }
            
            
            
        }

        // ── 更新对话时间 ──
        crate::db::try_with_db(|conn| {
            if let Err(e) = self.touch_conversation(conn) { tracing::error!(target:"db", error=%e, "touch conversation"); }
        });

        self.hook_register.emit(&AgentEvent::Stop { reason: if cancelled { "cancelled" } else { "completed" }.into() });

        // ── 构建 Static 消息 ──
        let segments_snapshot = segments_arc.lock().expect("segments_arc lock poisoned").clone();
        let tool_records_snapshot = tool_calls_arc.lock().expect("tool_calls_arc lock poisoned").clone();

        let content = build_content_from_segments(&segments_snapshot);
        let content = if content.is_empty() { _final_output.clone() } else { content };

        Ok(UiMessage::Static(ChatMessage {
            role: ChatRole::Assistant, content,
            timestamp: chrono::Local::now(),
            tool_calls: tool_records_snapshot,
            reasoning: final_reasoning,
            segments: segments_snapshot,
            content_html: String::new(),
        }))
    }

    /// 将 self.messages 转换为 DB 行（日后复用）
    pub fn to_message_rows(&self) -> Vec<crate::db::metadata::message::MessageRow> {
        self.messages
            .iter()
            .filter(|m| matches!(m.role, LlmRole::User | LlmRole::Assistant | LlmRole::Tool))
            .map(|m| crate::db::metadata::message::MessageRow::from_llm(m, &self.conversation_id))
            .collect()
    }

    /// 将单条 llm::Message 持久化到 messages 表（不删旧消息）。
    pub fn save_message(&self, conn: &rusqlite::Connection, msg: &llm::Message) -> rusqlite::Result<()> {
        use crate::db::metadata::message;
        let row = message::MessageRow::from_llm(msg, &self.conversation_id);
        message::create(conn, &self.conversation_id, &row)?;
        Ok(())
    }

    /// 单独更新 conversations.updated_at 为当前时间。
    pub fn touch_conversation(&self, conn: &rusqlite::Connection) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![chrono::Local::now().to_rfc3339(), self.conversation_id],
        )?;
        Ok(())
    }
}

// ── Arc 辅助操作 ─────────────────────────────────────────────────────────────

fn inc_version_atomic(version: &Arc<AtomicU64>) {
    version.fetch_add(1, Ordering::Relaxed);
}

fn push_text(segments: &Arc<Mutex<Vec<StreamSegment>>>, text: &str, version: &Arc<AtomicU64>) {
    let mut segs = segments.lock().expect("segments lock poisoned");
    match segs.last_mut() {
        Some(StreamSegment::Text(t)) => t.push_str(text),
        _ => segs.push(StreamSegment::Text(text.to_string())),
    }
    drop(segs);
    inc_version_atomic(version);
}

fn push_reasoning(segments: &Arc<Mutex<Vec<StreamSegment>>>, text: &str, version: &Arc<AtomicU64>) {
    let mut segs = segments.lock().expect("segments lock poisoned");
    match segs.last_mut() {
        Some(StreamSegment::Reasoning(t)) => t.push_str(text),
        _ => segs.push(StreamSegment::Reasoning(text.to_string())),
    }
    drop(segs);
    inc_version_atomic(version);
}

fn push_tool_marker(segments: &Arc<Mutex<Vec<StreamSegment>>>, version: &Arc<AtomicU64>) {
    segments.lock().expect("segments lock poisoned").push(StreamSegment::ToolCall);
    inc_version_atomic(version);
}

fn build_content_from_segments(segments: &[StreamSegment]) -> String {
    let mut content = String::new();
    for seg in segments {
        if let StreamSegment::Text(t) = seg { content.push_str(t); }
    }
    content
}

pub fn defautlt_main_agent_prompt() -> String {
    "You are a helpful assistant. Current workspace: ${workspace_path}.".to_string()
}
