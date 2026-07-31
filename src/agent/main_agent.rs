// ── AgentRun 主循环 ──
//
// 一次 accept_message 执行被拆成有名字的步骤（阶段 3）：
//   accept_message（编排）→ stream_round（一轮流式）→ execute_tools/execute_tool（工具）
//   → finish（收尾构建 Static）
// 审批走 ApprovalGate 策略链（可替换）；事件经 mpsc 发给 UI。

use futures::StreamExt;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use super::approval::ApprovalCtx;
use super::hook::{AgentEvent, DeltaKind};
use super::{Agent, AgentRun, ToolContext};
use crate::model::{ChatMessage, Role as ChatRole, StreamSegment, ToolCallRecord, ToolCallStatus, UiMessage};
use crate::permission::Decision;
use llm::{Chunk, Message, Request, Role as LlmRole, Usage};

// ── select! 辅助枚举 ────────────────────────────────────────────────────────

enum StreamOrCancel<T> {
    Chunk(T),
    Cancelled,
}

/// 一轮流式的结果
enum RoundOutcome {
    /// 有工具调用，继续执行工具
    Continue,
    /// 无工具且完成，正常收尾
    Finish,
    /// 流式出错，直接收尾（不计数）
    Abort,
    /// 被取消
    Cancelled,
}

// ── AgentRun 执行步骤 ───────────────────────────────────────────────────

impl AgentRun {
    /// 接受用户输入并运行 agent 循环。返回已完成的 UiMessage::Static。
    ///
    /// 调用前必须先调用 create_streaming() 初始化内部流式状态。
    pub async fn accept_message(
        &mut self,
        user_input: String,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<UiMessage> {

        let (segments_arc, approval_arc) =
            {
                self.create_streaming();
                let ss = self.streaming_handle.as_ref().unwrap_or_else(|| {
                    panic!("create_streaming() must be called first")
                });
                (
                    ss.segments.clone(),
                    ss.approval_tx.clone(),
                )
            };
        
        self.agent.hook_register.emit(&AgentEvent::UserPromptSubmit { prompt: user_input.clone() });

        // ── 前缀注入 目前只有 Plan Mode ──
        let augmented_input = self.agent.handler.prompt_before_user_message();
        if let Some(augmented_input) = augmented_input {
            self.agent.push_message(Message { role: LlmRole::System, content: Some(augmented_input.to_string()), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;
        }
        // 用户输入
        self.agent.push_message(Message { role: LlmRole::User, content: Some(user_input), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;

        if let Some(pre_prompt) = self.agent.handler.prompt_pre_loop() {
            self.agent.push_message(Message { role: LlmRole::System, content: Some(pre_prompt), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;
        }

        self.start_loop();

        // ReAct Loop（编排：只分发，不实现细节）
        loop {
            match self.stream_round(&cancel_token, &segments_arc).await? {
                RoundOutcome::Cancelled | RoundOutcome::Abort => break,
                RoundOutcome::Finish => { self.round_end(); break; }
                RoundOutcome::Continue => {
                    self.execute_tools(&cancel_token, &segments_arc, &approval_arc).await?;
                    if self.cancelled { break; }
                    self.round_end();
                }
            }
        }

        Ok(self.finish(&segments_arc))
    }

    /// 一轮：流式输出 + usage 持久化 + assistant 消息入史，返回本轮走向。
    async fn stream_round(
        &mut self,
        cancel_token: &CancellationToken,
        segments_arc: &Arc<Mutex<Vec<StreamSegment>>>,
    ) -> anyhow::Result<RoundOutcome> {
        self.round_start();
        let mut have_tool_calls = false;

        // Agent 回合日志
        tracing::debug!(
            target: "agent",
            round = self.round,
            messages = self.agent.messages.len(),
            tools = self.agent.registry.schemas().len(),
            "agent round"
        );

        let req = Request {
            messages: self.agent.messages.clone(),
            tools: self.agent.registry.schemas(),
            temperature: self.agent.temperature,
            max_tokens: self.agent.max_tokens.unwrap_or(65536),
        };

        let stream = match self.agent.provider.stream(&req).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Stream error: {e}");
                push_text(segments_arc, &msg);
                self.emit(AgentEvent::Error { message: msg.clone() });
                self.final_output = msg;
                return Ok(RoundOutcome::Abort);
            }
        };

        tokio::pin!(stream);

        let mut output = String::new();
        let mut reasoning = String::new();
        let mut last_usage: Option<llm::Usage> = None;

        // stream loop
        loop {
            let result = tokio::select! {
                _ = cancel_token.cancelled() => StreamOrCancel::Cancelled,
                r = stream.next() => StreamOrCancel::Chunk(r),
            };

            match result {
                StreamOrCancel::Cancelled => { self.cancelled = true; break; }
                StreamOrCancel::Chunk(None) => break,
                StreamOrCancel::Chunk(Some(Ok(Chunk::Text(t)))) => {
                    output.push_str(&t);
                    push_text(segments_arc, &t);
                    self.emit(AgentEvent::StreamDelta { kind: DeltaKind::Text });
                }
                StreamOrCancel::Chunk(Some(Ok(Chunk::Reasoning { text, .. }))) => {
                    reasoning.push_str(&text);
                    push_reasoning(segments_arc, &text);
                    self.emit(AgentEvent::StreamDelta { kind: DeltaKind::Reasoning });
                }
                StreamOrCancel::Chunk(Some(Ok(Chunk::ToolCallComplete(tc)))) => {
                    have_tool_calls = true;
                    self.pending_tool_calls.push(tc.clone());
                }
                StreamOrCancel::Chunk(Some(Ok(Chunk::Usage(usage)))) => {
                    if let Some(ref last) = last_usage {
                        last_usage = Some(Usage {
                            prompt_tokens: last.prompt_tokens + usage.prompt_tokens,
                            completion_tokens: last.completion_tokens + usage.completion_tokens,
                            reasoning_tokens: last.reasoning_tokens + usage.reasoning_tokens,
                            total_tokens: last.total_tokens + usage.total_tokens,
                            cache_hit_tokens: last.cache_hit_tokens + usage.cache_hit_tokens,
                            cache_miss_tokens: last.cache_miss_tokens + usage.cache_miss_tokens,
                            finish_reason: format!("{}\n{}", last.finish_reason, usage.finish_reason),
                        });
                    } else {
                        last_usage = Some(usage);
                    }
                }
                StreamOrCancel::Chunk(Some(Ok(_))) => {} // Start/Delta
                StreamOrCancel::Chunk(Some(Err(e))) => {
                    let msg = format!("Stream error: {e}");
                    output.push_str(&msg);
                    push_text(segments_arc, &msg);
                    self.emit(AgentEvent::Error { message: msg.clone() });
                    break;
                }
            }
        }
        // stream loop end

        // 持久化 token 用量
        if let Some(ref usage) = last_usage {
            let usaget_record = crate::model::TokenUsageRecord {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                total_tokens: usage.total_tokens,
                cache_hit_tokens: usage.cache_hit_tokens,
                cache_miss_tokens: usage.cache_miss_tokens,
            };
            match crate::db::get_db().lock() {
                Ok(guard) => {
                    if let Err(e) = crate::db::metadata::conversation::accumulate_usage(
                        &guard, &self.agent.conversation_id,
                        &usaget_record,
                    ) {
                        tracing::warn!(target: "dashboard", error = %e, "accumulate_usage failed (cancelled)");
                    }
                }
                Err(e) => {
                    tracing::warn!(target: "dashboard", error = %e, "db lock failed for accumulate_usage (cancelled)");
                }
            }
            self.last_usage = Some(usaget_record);
        }

        if self.cancelled {
            let output_empty = output.is_empty();
            let reasoning_empty = reasoning.is_empty();
            self.final_output = output.clone();
            self.final_reasoning.push_str(&reasoning);
            if !output_empty || !self.pending_tool_calls.is_empty() {
                let msg = Message {
                    role: LlmRole::Assistant,
                    content: if output_empty { None } else { Some(output.clone()) },
                    reasoning_content: if reasoning_empty { None } else { Some(reasoning.clone()) },
                    tool_calls: if self.pending_tool_calls.is_empty() { Vec::new() } else { self.pending_tool_calls.clone() },
                    timestamp: Some(chrono::Utc::now()),
                    ..Default::default()
                };
                self.agent.push_message(msg)?;
            }
            return Ok(RoundOutcome::Cancelled);
        }

        // Assistant 消息入 self.agent.messages
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
            self.agent.push_message(msg)?;
        }

        self.final_output = output;
        self.final_reasoning.push_str(&reasoning);

        if !have_tool_calls {
            if let Some(reason) = self.agent.handler.can_finish() {
                self.agent.push_message(Message { role: LlmRole::System, content: Some(reason), timestamp: Some(chrono::Utc::now()), ..Default::default() })?;
                return Ok(RoundOutcome::Continue);
            }
            return Ok(RoundOutcome::Finish);
        }
        Ok(RoundOutcome::Continue)
    }

    /// 执行本轮全部工具调用。
    async fn execute_tools(
        &mut self,
        cancel_token: &CancellationToken,
        segments_arc: &Arc<Mutex<Vec<StreamSegment>>>,
        approval_arc: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
    ) -> anyhow::Result<()> {
        for i in 0..self.pending_tool_calls.len() {
            if self.cancelled { break; }
            let tool_call = self.pending_tool_calls[i].clone();
            self.execute_tool(&tool_call, cancel_token, segments_arc, approval_arc).await?;
        }
        Ok(())
    }

    /// 执行单个工具调用（记录 push → 决策 → 审批 → 执行 → 回写），完整单元。
    async fn execute_tool(
        &mut self,
        tool_call: &llm::ToolCall,
        cancel_token: &CancellationToken,
        segments_arc: &Arc<Mutex<Vec<StreamSegment>>>,
        approval_arc: &Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
    ) -> anyhow::Result<()> {
        let tool_name = tool_call.name.clone();
        self.agent.hook_register.emit(&AgentEvent::PreToolUse {
            tool_name: tool_name.clone(),
            args: serde_json::from_str(&tool_call.arguments).unwrap_or_default(),
        });

        if let Some(tool) = self.agent.registry.get(&tool_name) {
            let args: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or_default();

            // push 工具记录（内嵌进 segments —— 单一数据源）
            {
                let rec = ToolCallRecord {
                    tool_name: tool_name.clone(), args: args.clone(),
                    result: None, status: ToolCallStatus::Running, approval_reason: None,
                };
                segments_arc.lock().expect("segments_arc lock poisoned")
                    .push(StreamSegment::ToolCall(rec));
            }
            self.emit(AgentEvent::ToolCallStart { tool_name: tool_name.clone(), args: args.clone() });

            let ctx = ToolContext { call_id: tool_call.id.clone(), plan_mode: self.agent.handler.action_mode(), handler: self.agent.handler.clone(), progress: None, main_conversation_id: self.agent.conversation_id.clone(), project_id: self.agent.project_id.clone(), cancel_token: Some(cancel_token.clone()) };
            let decision = tool.pre_check(&ctx, &args);
            let is_denied = matches!(decision, Decision::Deny(_));
            let result = match decision {
                Decision::Allow => {
                    let exec = tool.execute(&ctx, &args);
                    tokio::pin!(exec);
                    tokio::select! {
                        _ = cancel_token.cancelled() => {
                            self.cancelled = true;
                            Err("cancelled by user".into())
                        }
                        r = &mut exec => r,
                    }
                }
                Decision::Ask => {
                    let reason = format!("{} needs approval", tool_name);
                    {
                        let mut segs = segments_arc.lock().expect("segments_arc lock poisoned");
                        if let Some(rec) = find_record(&mut segs, &tool_name, |r| r.status == ToolCallStatus::Running) {
                            rec.status = ToolCallStatus::AwaitingApproval { reason: reason.clone() };
                            rec.approval_reason = Some(reason.clone());
                        }
                    }
                    self.emit(AgentEvent::ApprovalRequested {
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                        reason: reason.clone(),
                    });

                    // 审批走策略链（可替换；默认：弹窗让用户点）
                    let approval_ctx = ApprovalCtx {
                        tool_name: tool_name.clone(),
                        args: args.clone(),
                        reason: reason.clone(),
                        cancel: cancel_token.clone(),
                        approval_tx: approval_arc.clone(),
                    };
                    match self.approval_gate.request(&approval_ctx).await {
                        Decision::Allow => {
                            // 立即更新状态为 Running，让 UI 及时响应
                            {
                                let mut segs = segments_arc.lock().expect("segments_arc lock poisoned");
                                if let Some(rec) = find_record(&mut segs, &tool_name, |r| matches!(r.status, ToolCallStatus::AwaitingApproval { .. })) {
                                    rec.status = ToolCallStatus::Running;
                                }
                            }
                            *approval_arc.lock().expect("approval_arc lock poisoned") = None;
                            // 审批通过，进入执行：再次广播 ToolCallStart 让 UI 刷新
                            self.emit(AgentEvent::ToolCallStart { tool_name: tool_name.clone(), args: args.clone() });
                            let exec = tool.execute(&ctx, &args);
                            tokio::pin!(exec);
                            tokio::select! {
                                _ = cancel_token.cancelled() => {
                                    self.cancelled = true;
                                    Err("cancelled by user".into())
                                }
                                r = &mut exec => r,
                            }
                        }
                        Decision::Deny(_msg) if cancel_token.is_cancelled() => {
                            self.cancelled = true;
                            Err("cancelled by user".into())
                        }
                        Decision::Deny(msg) => Err(msg),
                        Decision::Ask => Err("approval chain could not decide".into()),
                    }
                }
                Decision::Deny(msg) => Err(msg),
            };

            // 更新 tool record 状态（segments 内嵌记录，锁内直接改）
            {
                let mut segs = segments_arc.lock().expect("segments_arc lock poisoned");
                if let Some(rec) = find_record(&mut segs, &tool_name, |r| {
                    r.status == ToolCallStatus::Running || matches!(r.status, ToolCallStatus::AwaitingApproval { .. })
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
            self.emit(AgentEvent::ToolCallEnd { tool_name: tool_name.clone(), result: result.clone() });

            self.agent.hook_register.emit(&AgentEvent::PostToolUse { tool_name: tool_name.clone(), result: result.clone() });
            let tool_message = Message {
                role: LlmRole::Tool,
                content: Some(match &result { Ok(tr) => tr.output.clone(), Err(e) => format!("error: {e}") }),
                tool_call_id: Some(tool_call.id.clone()), tool_name: Some(tool_name),
                timestamp: Some(chrono::Utc::now()),
                ..Default::default()
            };
            self.agent.push_message(tool_message)?;
        }
        Ok(())
    }

    /// 收尾：touch conversation + Stop 事件 + 构建 Static 消息。
    fn finish(&mut self, segments_arc: &Arc<Mutex<Vec<StreamSegment>>>) -> UiMessage {
        // ── 更新对话时间 ──
        crate::db::try_with_db(|conn| {
            if let Err(e) = self.agent.touch_conversation(conn) { tracing::error!(target:"db", error=%e, "touch conversation"); }
        });

        let stop_reason = if self.cancelled { "cancelled" } else { "completed" };
        self.agent.hook_register.emit(&AgentEvent::Stop { reason: stop_reason.into() });
        self.emit(AgentEvent::Stop { reason: stop_reason.into() });

        // ── 构建 Static 消息 ──
        let segments_snapshot = segments_arc.lock().expect("segments_arc lock poisoned").clone();

        let content = build_content_from_segments(&segments_snapshot);
        let content = if content.is_empty() { self.final_output.clone() } else { content };

        let result = UiMessage::Static(ChatMessage {
            role: ChatRole::Assistant, content,
            timestamp: chrono::Local::now(),
            reasoning: self.final_reasoning.clone(),
            segments: segments_snapshot,
            content_html: String::new(),
        });

        // 本轮流式状态已结束：清理 streaming_handle，避免下一轮 create_streaming()
        // 复用上一轮的 segments，导致流式回显之前所有 agent 的返回消息。
        self.streaming_handle = None;

        result
    }
}

// ── Agent 配置态方法（跨轮，不依赖 Run）──────────────────────────────

impl Agent {
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

fn push_text(segments: &Arc<Mutex<Vec<StreamSegment>>>, text: &str) {
    let mut segs = segments.lock().expect("segments lock poisoned");
    match segs.last_mut() {
        Some(StreamSegment::Text(t)) => t.push_str(text),
        _ => segs.push(StreamSegment::Text(text.to_string())),
    }
}

fn push_reasoning(segments: &Arc<Mutex<Vec<StreamSegment>>>, text: &str) {
    let mut segs = segments.lock().expect("segments lock poisoned");
    match segs.last_mut() {
        Some(StreamSegment::Reasoning(t)) => t.push_str(text),
        _ => segs.push(StreamSegment::Reasoning(text.to_string())),
    }
}

/// 在 segments 中按条件定位工具记录（从末尾往前找最近的一条）
fn find_record<'a>(
    segs: &'a mut [StreamSegment],
    tool_name: &str,
    pred: impl Fn(&ToolCallRecord) -> bool,
) -> Option<&'a mut ToolCallRecord> {
    segs.iter_mut().rev().find_map(|s| match s {
        StreamSegment::ToolCall(r) if r.tool_name == tool_name && pred(r) => Some(r),
        _ => None,
    })
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
