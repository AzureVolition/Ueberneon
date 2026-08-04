// ── AgentRun：单次执行上下文（方案 B） ──
//
// Agent（配置态 + 消息历史，跨轮存活）与"一次 accept_message 执行"分离：
// AgentRun 持有 Agent 的**所有权**（无借用、无生命周期），可 move / spawn；
// 运行态（流式句柄 / 挂起工具 / 轮次 / token 用量）都挂在 Run 上，
// Run 结束即销毁 —— 跨轮状态泄漏在结构上不可能发生。

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::model::{StreamingState, UiMessage};
use llm::ToolCall;

use super::approval::{ApprovalChain, ApprovalGate, UserApprovalGate};
use super::hook::AgentEvent;
use super::Agent;

/// Agent 运行状态（变体 A：状态可查，UI 可通过事件/字段精确感知）
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum AgentState {
    /// 未开始
    Idle,
    /// LLM 流式输出中
    Streaming,
    /// 执行工具
    Executing,
    /// 停在审批（仍阻塞等待，但状态对外可见）
    WaitingApproval,
    /// 正常完成
    Done,
    /// 被取消
    Cancelled,
    /// 出错（流式错误等）
    Error,
}

/// 审批挂起点（可查询、可序列化，为断点铺路）
#[derive(Clone)]
pub struct PendingApproval {
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub reason: String,
}

/// 审批结果注入后待执行的工具（resolve_approval 只存决策，next_step 驱动执行）
pub struct PendingResume {
    pub req: PendingApproval,
    pub decision: crate::permission::Decision,
}

/// run 一路跑到"需要外部介入"或"结束"时返回给驱动者的结果
pub enum Blocked {
    Approval(PendingApproval, tokio::sync::oneshot::Receiver<bool>),
    Done(StopReason),
}

/// 运行结束原因
pub enum StopReason {
    Completed,
    Cancelled,
    Error,
}

pub struct AgentRun {
    /// 整个 Agent（配置 + 历史 + provider/registry/handler 等）
    pub agent: Agent,
    /// 内部流式状态（由 create_streaming() 初始化）
    pub streaming_handle: Option<StreamingState>,
    /// 挂起的工具调用
    pub pending_tool_calls: Vec<ToolCall>,
    /// 最近一次 LLM 交互的 token 用量（accept_message 结束后填充）
    pub last_usage: Option<crate::model::TokenUsageRecord>,
    /// 循环数
    pub round: u32,
    /// 审批策略链（Ask 之后怎么决策，可替换）
    pub approval_gate: Box<dyn ApprovalGate>,
    /// 是否被取消（accept_message 全程有效，拆方法后需要跨方法共享）
    pub cancelled: bool,
    /// 最终输出兜底（收尾构建 Static 用）
    pub final_output: String,
    /// 最终推理内容（收尾构建 Static 用）
    pub final_reasoning: String,
    /// 取消令牌（外部注入；方法内取用，不跨方法传参）
    pub cancel_token: CancellationToken,
    /// 当前运行状态
    pub state: AgentState,
    /// 工具批次断点游标：已执行到第几个工具（审批恢复后续跑，防重复执行）
    pub tool_index: usize,
    /// 审批结果注入后待执行的工具（resolve_approval 只存决策，next_step 驱动执行）
    pub pending_resume: Option<PendingResume>,
    /// 事件通道（执行节点 emit，UI/调用方订阅）
    /// unbounded：高频流式下也不丢事件（丢事件会导致 UI 漏掉审批卡/tick 而卡死）
    events: mpsc::UnboundedSender<AgentEvent>,
}

impl AgentRun {
    /// 从 Agent 取走所有权，进入执行态。
    /// 返回 (Run, 事件接收端)——调用方订阅事件以驱动 UI。
    /// cancel_token 由调用方创建并注入（Run 内持一份 clone）。
    pub fn new(agent: Agent, cancel_token: CancellationToken) -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let run = Self {
            agent,
            streaming_handle: None,
            pending_tool_calls: Vec::new(),
            last_usage: None,
            round: 0,
            approval_gate: Box::new(ApprovalChain::new(vec![Box::new(UserApprovalGate)])),
            cancelled: false,
            final_output: String::new(),
            final_reasoning: String::new(),
            cancel_token,
            state: AgentState::Idle,
            tool_index: 0,
            pending_resume: None,
            events: tx,
        };
        (run, rx)
    }

    /// 向事件通道投递事件（unbounded：不阻塞、不丢；无订阅者时静默丢弃）
    pub fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }

    /// 更新运行状态并广播 StateChanged 事件（变体 A：状态可查）
    pub fn set_state(&mut self, state: AgentState) {
        self.state = state;
        self.emit(AgentEvent::StateChanged { state });
    }

    /// 归还 Agent 所有权（执行结束后恢复配置态）。
    pub fn into_agent(self) -> Agent {
        self.agent
    }

    /// 创建内部流式状态，返回 UiMessage::Streaming 供 UI 显示。
    /// 必须先调用此方法，再调用 accept_message。
    pub fn create_streaming(&mut self) -> UiMessage {
        if self.streaming_handle.is_none() {
            let state = StreamingState {
                segments: Arc::new(Mutex::new(Vec::new())),
                approval_tx: Arc::new(Mutex::new(None)),
            };
            self.streaming_handle = Some(state);
        }

        UiMessage::Streaming(self.streaming_handle.unwrap().clone())
    }

    /// 从 streaming_handle 取出共享 Arc（方法内按需取，无需跨方法传参）
    pub fn arcs(&self) -> (
        Arc<Mutex<Vec<crate::model::StreamSegment>>>,
        Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
    ) {
        let ss = self.streaming_handle.as_ref().expect("create_streaming() must be called first");
        (ss.segments.clone(), ss.approval_tx.clone())
    }

    pub fn start_loop(&mut self) {
        self.round = 0;
    }

    pub fn round_start(&mut self) {
        self.round += 1;
    }

    pub fn round_end(&mut self) {
        // ── stall_count 计数与催促 ──
        let mut plan_guard = self.agent.handler.current_plan.lock().expect("current_plan lock poisoned");
        if let Some(ref mut plan) = *plan_guard {
            let completed_this_round = self.pending_tool_calls.iter().any(|tc| tc.name == "CompleteStep");
            if completed_this_round {
                plan.stall_count = 0;
            } else {
                plan.stall_count += 1;
                if plan.stall_count >= 3 {
                    // 注入催促系统消息
                    let nudge = llm::Message {
                        role: llm::Role::System,
                        content: Some(super::prompts::plan::STALL_NUDGE_SUFFIX.to_string()),
                        timestamp: Some(chrono::Utc::now()),
                        ..Default::default()
                    };
                    // 直接 push 到下一条，让 LLM 看到催促
                    self.agent.messages.push(nudge);
                    plan.stall_count = 0; // 重置避免重复催促
                }
            }
        }
        // 清空本轮工具调用，避免下轮重复执行
        self.pending_tool_calls.clear();
    }
}
