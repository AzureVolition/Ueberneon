use tokio::sync::oneshot;
use super::Static;
use super::InterruptState;
use super::Agent;
use crate::model::{ChatMessage, Role as ChatRole, StreamSegment, ToolCallRecord, ToolCallStatus, UiMessage};
use crate::permission::Decision;
use crate::model::{ StreamingState};
use crate::tools::internal::common::checkable_tool::CheckableTool;
use llm::{Chunk, Message, Request,ToolCall, Role as LlmRole, Usage};
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::Mutex;

use tokio::sync::mpsc;




pub struct Running<T> {
    
    pub streaming: T,
    pub messages: UiMessage::Streaming,
    /// 运行时控制句柄（含 action_mode / agent_mode / current_plan）
    pub handler: AgentHandler,
    /// 工具注册表
    pub registry: Arc<Registry>,
    /// 事件钩子注册表
    pub hook_register: HookRegister,
    /// 内部流式状态（由 create_streaming() 初始化）
    pub streaming_handle: StreamingState,
    /// 最近一次 LLM 交互的 token 用量（accept_message 结束后填充）
    pub last_usage: Option<crate::model::TokenUsageRecord>,
    /// 循环数
    pub round: u32,
    /// 审批策略链（Ask 之后怎么决策，可替换）
    pub approval_gate: Box<dyn ApprovalGate>,
    /// 是否被取消（accept_message 全程有效，拆方法后需要跨方法共享）
    pub stopped: bool,
    
    /// 取消令牌（外部注入；方法内取用，不跨方法传参）
    /// todo 需要移动到handle
    pub cancel_token: CancellationToken,
    /// 当前运行状态
    pub state: AgentState,

    /// 审批结果注入后待执行的工具（resolve_approval 只存决策，next_step 驱动执行）
    pub pending_resume: Option<PendingResume>,
    /// 事件通道（执行节点 emit，UI/调用方订阅）
    /// unbounded：高频流式下也不丢事件（丢事件会导致 UI 漏掉审批卡/tick 而卡死）
    events: mpsc::UnboundedSender<AgentEvent>,
}

impl Running<Streaming> {
    pub async fn init(req: &Request, handler: AgentHandler, registry: Arc<Registry>, hook_register: HookRegister)
                    -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {

        let state = StreamingState {
            segments: Arc::new(Mutex::new(Vec::new())),
            approval_tx: Arc::new(Mutex::new(None)),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let run = Self {
            streaming: Streaming::init(req).await,
            handler,
            registry,
            hook_register,
            state,
            last_usage: None,
            round: 0,
            approval_gate: Box::new(ApprovalChain::new(vec![Box::new(UserApprovalGate)])),
            stopped: false,
        };
        (run, rx)
    }

    pub async fn streaming(&mut self) -> UiMessage {
        UiMessage::Streaming(self.streaming_handle.clone())
    }
}

impl<T> Agent<Running<T>> {
    
    /// 向事件通道投递事件（unbounded：不阻塞、不丢；无订阅者时静默丢弃）
    pub fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }
}


pub struct Streaming {
    pub stream: ChunkStream,
    pub content: String,
    pub reason_content: String,
    pub tool_id_list: Vec<String>,
    pub segments: Arc<Mutex<Vec<StreamSegment>>>,
}

impl Streaming {
    pub async fn init(req: &Request) -> Self {
        let stream = match self.agent.provider.stream(&req).await {
            Ok(s) => s,
            Err(e) => {
                let msg = format!("Stream error: {e}");
                push_text(&segments_arc, &msg);
                self.emit(AgentEvent::Error { message: msg.clone() });
                self.final_output = msg;
                self.set_state(AgentState::Error);
                return Ok(RoundOutcome::Abort);
            }
        };

        tokio::pin!(stream);
        Self {
            stream,
            content: String::new(),
            reason_content: String::new(),
            tool_id_list: Vec::new(),
        }
    }
}

pub struct Executing {
    pub tool_id_list: Vec<String>,
    ctx: ToolContext,
    approval_rx: mpsc::Receiver<(String, bool)>,
    waiting_approval: Option<(String, oneshot::Sender<bool>)>,
}

impl Executing {
    pub fn init(tool_id_list: Vec<String>) -> (Self, mpsc::Sender<(String, bool)>) {
        let (tx, mut rx) = mpsc::channel(32);
        (Self {
            tool_id_list,
            approval_rx: rx,
            waiting_approval: None,
        }, tx)
    }
}

impl Agent<Running<Executing>> {
    pub async fn execute(&mut self) -> Result<Agent<Static>, InterruptState> {
        
        tokio::spawn(async move {
            while let Some((tool_call_id, approved)) = self.running.streaming.approval_rx.recv().await {
                if self.cancelled { break; }
                if let Some((id, sender)) = self.running.streaming.waiting_approval.take() && id == tool_call_id {
                   sender.send(approved);
                   self.running.streaming.waiting_approval = None;
                }else {
                    if let Some(tc) = self.running.messages[0]
                        .lock()
                        .expect("segments lock poisoned")
                        .iter_mut()
                        .find(|tc| matches!(tc, StreamSegment::ToolCall(record) if record.id == tool_call_id))
                    {
                        tc.status = ToolCallStatus::Pending;
                    }else {
                        panic!("tool call not found");
                    }
                }
                
            }
        });
        // let tool_call_list = self.running.messages[0].lock().expect("segments lock poisoned").iter_mut().filter(|tc| matches!(tc, StreamSegment::ToolCall(_))).collect::<Vec<_>>();
        for i in 0..self.running.streaming.tool_id_list.len() {
            if self.cancelled { break; }
            let tc_id = self.running.streaming.tool_id_list[i].clone();
            if let Some(tc) = self.get_tool_call_by_id(&tc_id) {
                if tc.status != ToolCallStatus::Pending && tc.status != ToolCallStatus::AwaitingApproval {
                    return Err(InterruptState::Error("tool call status not pending or waiting approval".to_string()));
                }
                if tc.status == ToolCallStatus::AwaitingApproval {
                    let rx = self.create_notify(&tc_id);
                    drop(tc);
                    let (id, approved) = rx.recv().await?;
                    if let Some(new_tc) = self.get_tool_call_by_id(&tc_id) {
                        tc = new_tc;
                    }
                    if !approved {
                        tc.status = ToolCallStatus::Denied("denied by user".to_string());
                        continue;
                    }
                }
                tc.status = ToolCallStatus::Running;
                let Some(tool) = self.agent.registry.get(&tc.tool_name) else {
                    return Err(InterruptState::Error("tool not registered".to_string()));
                };
                let result = self.execute_with_cancel(&tool, &tc.args).await?;
                tc.status = ToolCallStatus::Success;
                tc.result = Some(result.to_string());
            } else {
                return Err(InterruptState::Error("tool call not found".to_string()));
            }
        }

        self.running.executing.approval_rx.close();
        Ok()
    }


    fn get_tool_call_by_id(&mut self, tool_call_id: &str) -> Option<&mut ToolCallRecord> {
        self.running.streaming.segments_arc.lock()
            .expect("segments lock poisoned")
            .iter_mut()
            .find(|tc| matches!(tc, StreamSegment::ToolCall(record) if record.id == tool_call_id))
    }

    
    
    /// 单工具执行（含取消拦截）。
    async fn execute_with_cancel(
        &mut self,
        tool: &Arc<dyn CheckableTool + Send + Sync>,
        args: &serde_json::Value,
    ) -> Result<ToolResult, InterruptState> {
        let cancel_token = self.cancel_token.clone();
        let exec = tool.execute(&ctx, args);
        tokio::pin!(exec);
        tokio::select! {
            _ = cancel_token.cancelled() => {
                self.stopped = true;
                Err(InterruptState::Cancelled)
            }
            r = &mut exec => match r {
                Ok(result) => Ok(result),
                Err(e) => Err(InterruptState::Error(e.to_string())),
            },
        }
    }
    
    async fn finalize_tool(
        &mut self,
        call_id: &str,
        tool_name: &str,
        result: Result<ToolResult, String>,
        denied: bool,
    ) -> anyhow::Result<()> {
        let (segments_arc, approval_arc) = self.running.streaming_handle;
        // 更新 tool record 状态（segments 内嵌记录，锁内直接改）
        {
            let mut segs = segments_arc.lock().expect("segments_arc lock poisoned");
            if let Some(rec) = find_record(&mut segs, tool_name, |r| {
                r.status == ToolCallStatus::Running || matches!(r.status, ToolCallStatus::AwaitingApproval { .. })
            }) {
                rec.result = Some(match &result { Ok(tr) => tr.output.clone(), Err(e) => e.clone() });
                rec.status = match &result {
                    Ok(_) => ToolCallStatus::Success,
                    Err(e) if denied || e.starts_with("denied by user:") || e == "approval channel closed" => {
                        ToolCallStatus::Denied(e.clone())
                    }
                    Err(e) => ToolCallStatus::Failed(e.clone()),
                };
            }
        }
        *approval_arc.lock().expect("approval_arc lock poisoned") = None;
        self.emit(AgentEvent::ToolCallEnd { tool_name: tool_name.into(), result: result.clone() });

        self.agent.hook_register.emit(&AgentEvent::PostToolUse { tool_name: tool_name.into(), result: result.clone() });
        let tool_message = Message {
            role: LlmRole::Tool,
            content: Some(match &result { Ok(tr) => tr.output.clone(), Err(e) => format!("error: {e}") }),
            tool_call_id: Some(call_id.to_string()), tool_name: Some(tool_name.to_string()),
            timestamp: Some(chrono::Utc::now()),
            ..Default::default()
        };
        self.agent.push_message(tool_message)?;
        Ok(())
    }
    
    fn create_notify(&mut self, tool_call_id: &str) -> oneshot::Receiver<(String, bool)> {
        let (tx, rx) = oneshot::channel();
        self.running.executing.waiting_approval = Some(tx);
        rx
    }
} 
 


impl Agent<Running<Streaming>> {

    pub async fn stream_message(self) -> Result<Option<Agent<Running<Executing>>>, InterruptState> {
        self.running.streaming.segments_arc = self.streaming_handle.as_ref().expect("create_streaming() must be called first").segments.clone();
        loop {
            let result = tokio::select! {
                _ = self.running.streaming.cancel_token.cancelled() => StreamOrCancel::Cancelled,
                r = self.running.streaming.stream.next() => StreamOrCancel::Chunk(r),
            };
    
            match result {
                StreamOrCancel::Cancelled => { self.running.streaming.cancelled = true; return Ok(None); }
                StreamOrCancel::Chunk(None) => break,
                StreamOrCancel::Chunk(Some(Ok(Chunk::Text(t)))) => {
                    self.push_text(&t);
                    self.emit(AgentEvent::StreamDelta { kind: DeltaKind::Text });
                }
                StreamOrCancel::Chunk(Some(Ok(Chunk::Reasoning { text, .. }))) => {
                    self.push_reason(&text);
                    self.emit(AgentEvent::StreamDelta { kind: DeltaKind::Reasoning });
                }
                StreamOrCancel::Chunk(Some(Ok(Chunk::ToolCallComplete(tc)))) => {
                    self.push_tool_call(tc);
                
                }
                StreamOrCancel::Chunk(Some(Ok(Chunk::Usage(usage)))) => {
                    if let Some(ref last) = self.running.last_usage {
                        self.running.last_usage = Some(Usage {
                            prompt_tokens: last.prompt_tokens + usage.prompt_tokens,
                            completion_tokens: last.completion_tokens + usage.completion_tokens,
                            reasoning_tokens: last.reasoning_tokens + usage.reasoning_tokens,
                            total_tokens: last.total_tokens + usage.total_tokens,
                            cache_hit_tokens: last.cache_hit_tokens + usage.cache_hit_tokens,
                            cache_miss_tokens: last.cache_miss_tokens + usage.cache_miss_tokens,
                            finish_reason: format!("{}\n{}", last.finish_reason, usage.finish_reason),
                        });
                    } else {
                        self.running.last_usage = Some(usage);
                    }
                }
                StreamOrCancel::Chunk(Some(Ok(_))) => {} // Start/Delta
                StreamOrCancel::Chunk(Some(Err(e))) => {
                    let msg = format!("Stream error: {e}");
                    self.push_text(&msg);
                    self.emit(AgentEvent::Error { message: msg.clone() });
                    break;
                }
            }
        }

        // Assistant 消息入 self.agent.messages
        {
            let mut msg = Message {
                role: LlmRole::Assistant,
                content: if self.running.streaming.content.is_empty() { None } else { Some(self.running.streaming.content.clone()) },
                reasoning_content: if self.running.streaming.reason_content.is_empty() { None } else { Some(self.running.streaming.reason_content.clone()) },
                timestamp: Some(chrono::Utc::now()),
                tool_calls: if self.pending_tool_calls.is_empty() { Vec::new() } else { self.pending_tool_calls.clone() },
                ..Default::default()
            };
            self.agent.push_message(msg)?;
        }

        
        if self.running.streaming.tool_id_list.is_empty() {
            return Ok(None);
        }
        
        return Ok(Some(Self::wait_approval(self)));
    }
 
    pub fn to_execute(self) -> Result<(Agent<Running<Executing>>), InterruptState> {
        let (executing, tx) = Executing::init(self.running.streaming.tool_id_list);
        Ok(( Agent<Running<Executing>> {
            running: Running {
                streaming: executing,
                streaming_handle: self.running.streaming_handle,
                last_usage: self.running.last_usage,
                round: self.running.round,
                approval_gate: self.running.approval_gate,
                stopped: self.running.stopped,
                pending_resume: self.running.pending_resume,
                events: self.running.events,
                cancel_token: self.running.cancel_token,
                state: self.running.state,
            },
        }, tx.clone()))
    }
    
    pub fn push_text(&mut self, text: &str) {
        self.running.streaming.content.push_str(text);
        let mut segs = self.running.streaming.segments_arc.lock().expect("segments lock poisoned");
        match segs.last_mut() {
            Some(StreamSegment::Text(t)) => t.push_str(text),
            _ => segs.push(StreamSegment::Text(text.to_string())),
        }
    }

    pub fn push_reason(&mut self, text: &str) {
        self.running.streaming.reason_content.push_str(text);
        let mut segs = self.running.streaming.segments_arc.lock().expect("segments lock poisoned");
        match segs.last_mut() {
            Some(StreamSegment::Reasoning(t)) => t.push_str(text),
            _ => segs.push(StreamSegment::Reasoning(text.to_string())),
        }
    }

    pub fn push_tool_call(&mut self, tool_call: ToolCall) {
        let args: serde_json::Value = serde_json::from_str(&tool_call.arguments).unwrap_or_default();
        let rec = ToolCallRecord {
            id: tool_call.id,
            tool_name: tool_call.name, args: args,
            result: None, status: ToolCallStatus::Pending, approval_reason: None,
        };
        self.running.streaming.tool_id_list.push(rec.id.clone());
        let mut segs = self.running.streaming.segments_arc.lock().expect("segments lock poisoned");
        segs.push(StreamSegment::ToolCall(rec));
    }
    
}