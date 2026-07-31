// ── AgentRun：单次执行上下文（方案 B） ──
//
// Agent（配置态 + 消息历史，跨轮存活）与"一次 accept_message 执行"分离：
// AgentRun 持有 Agent 的**所有权**（无借用、无生命周期），可 move / spawn；
// 运行态（流式句柄 / 挂起工具 / 轮次 / token 用量）都挂在 Run 上，
// Run 结束即销毁 —— 跨轮状态泄漏在结构上不可能发生。

use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::model::{StreamingState, UiMessage};
use llm::ToolCall;

use super::approval::{ApprovalChain, ApprovalGate, UserApprovalGate};
use super::hook::AgentEvent;
use super::Agent;

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
    /// 事件通道（执行节点 emit，UI/调用方订阅）
    events: mpsc::Sender<AgentEvent>,
}

impl AgentRun {
    /// 从 Agent 取走所有权，进入执行态。
    /// 返回 (Run, 事件接收端)——调用方订阅事件以驱动 UI。
    pub fn new(agent: Agent) -> (Self, mpsc::Receiver<AgentEvent>) {
        let (tx, rx) = mpsc::channel(256);
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
            events: tx,
        };
        (run, rx)
    }

    /// 向事件通道投递事件（非阻塞；无订阅者时静默丢弃）
    pub fn emit(&self, event: AgentEvent) {
        let _ = self.events.try_send(event);
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

        let streaming = UiMessage::Streaming {
            segments: self.streaming_handle.as_ref().unwrap().segments.clone(),
            approval_tx: self.streaming_handle.as_ref().unwrap().approval_tx.clone(),
        };

        streaming
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
