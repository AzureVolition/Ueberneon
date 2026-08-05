// Agent 状态机驱动 → Dioxus signals 桥接层
//
// 新流程：UiContext 实现 PhaseObserver，由 AgentContext 白盒驱动状态机，
// 每个状态变换点直接访问完整 Agent<...> 对象做 UI 收尾。
//   Agent<Static>::accept_message → Agent<Running<Streaming>>::stream_message
//     → Agent<Running<Executing>>::execute → 回到配置态续跑下一轮
//
// 职责：
// 1. 事件 → tick 触发 UI 重渲染（事件消费 future 与状态变换同任务 select 并发，流式实时刷新）
// 2. 工具执行阶段把审批注入通道（mpsc Sender<(tool_call_id, bool)>）暴露给 UI，
//    UI 审批按钮点选后直接 send((tool_call_id, approved))
// 3. 变换结束（Done / InterruptState）→ 替换 Streaming 占位为 Static / 移除 + 注册回缓存

use dioxus::prelude::*;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::state_agent::manager::AgentManager;
use crate::state_agent::{
    ActionMode, Agent, AgentContext, AgentHandler, AgentMode, Executing, InterruptState,
    PhaseObserver, Running, Static, Streaming,
};
use crate::model::*;
use crate::ui::components::error::*;

/// bridge 运行时上下文 —— 打包 run_agent_loop 的所有入参（app.rs 构造点）。
pub struct BridgeContext {
    pub user_input: String,
    pub action_mode: ActionMode,
    pub agent_mode: AgentMode,
    pub runtimes: Signal<std::collections::HashMap<String, crate::ui::state::ConversationRuntime>>,
    pub is_streaming: Signal<bool>,
    pub streaming_project_id: Signal<Vec<String>>,
    pub project_id: String,
    pub cancel_token: CancellationToken,
    pub conversation_id: String,
    pub streaming_states: Arc<Mutex<std::collections::HashMap<String, UiMessage>>>,
    pub error_signal: Signal<ErrorSignal>,
    /// 审批注入通道发送端（按 conversation_id 键控，避免多对话并发串台）：
    /// 工具进入 Executing 阶段时写入，UI 审批按钮经它发送 (tool_call_id, approved)
    pub approval_tx: Signal<std::collections::HashMap<String, tokio::sync::mpsc::Sender<(String, bool)>>>,
}

/// UI 阶段观察者：由 AgentContext 在每个状态变换点调用，直接访问完整
/// `Agent<...>` 对象做 UI 收尾（占位/usage/审批通道/清理）。
struct UiContext {
    runtimes: Signal<std::collections::HashMap<String, crate::ui::state::ConversationRuntime>>,
    is_streaming: Signal<bool>,
    streaming_project_id: Signal<Vec<String>>,
    project_id: String,
    conversation_id: String,
    streaming_states: Arc<Mutex<std::collections::HashMap<String, UiMessage>>>,
    error_signal: Signal<ErrorSignal>,
    approval_tx: Signal<std::collections::HashMap<String, tokio::sync::mpsc::Sender<(String, bool)>>>,
    /// 流式占位用的 segments Arc（on_streaming 缓存，on_done 替换占位时消费）
    segments: Option<Arc<Mutex<Vec<StreamSegment>>>>,
    /// DB 恢复的累计 token 用量（on_streaming 建占位时初始化 runtime）
    db_usage: TokenUsageRecord,
    db_requests: u64,
}

impl PhaseObserver for UiContext {
    /// 进入流式态（accept_message 成功 / 每轮工具执行后续跑前）。
    /// 幂等：无占位才建（DB 恢复 usage/request_count 初始化 runtime）；
    /// 续跑轮先清上一轮审批通道残留（receiver 已被 execute 子线程 take）。
    fn on_streaming(&mut self, agent: &Agent<Running<Streaming>>) {
        self.approval_tx.write().remove(&self.conversation_id);

        let segments = agent.running.streaming_handle.segments.clone();
        // 无条件缓存：续跑轮为同一 Arc（Running → Running 传递同一 streaming_handle），
        // 保证 on_done 替换占位时一定拿得到 segments
        self.segments = Some(segments.clone());
        let need_placeholder = {
            let db_usage = self.db_usage.clone();
            let db_requests = self.db_requests;
            let mut rts = self.runtimes.write();
            let rt = rts.entry(self.conversation_id.clone()).or_insert_with(
                || crate::ui::state::ConversationRuntime {
                    accumulated_usage: db_usage,
                    request_count: db_requests,
                    ..Default::default()
                },
            );
            if rt.messages.iter().any(|m| matches!(m, UiMessage::Streaming { .. })) {
                false
            } else {
                rt.messages
                    .push(UiMessage::Streaming { segments: segments.clone() });
                true
            }
        };
        if need_placeholder {
            self.streaming_states
                .lock()
                .expect("streaming_states lock poisoned")
                .insert(self.conversation_id.clone(), UiMessage::Streaming { segments });
        }
    }

    /// 有工具待执行（Continue 后、execute 前）：本轮 usage 已写入、approval_tx 已设置。
    fn on_executing(&mut self, agent: &Agent<Running<Executing>>) {
        // usage/request 已发生，即时累加（dashboard 实时更新）
        if let Some(ref usage) = agent.agent.last_usage {
            accumulate_usage(self.runtimes, &self.conversation_id, usage);
        }
        {
            let mut all = self.runtimes.write();
            if let Some(rt) = all.get_mut(&self.conversation_id) {
                rt.request_count += 1;
            }
        }
        // 把审批注入通道交给 UI（Ask 时审批卡按钮经它发送 (tool_call_id, approved)）
        if let Some(tx) = agent.running.approval_tx.clone() {
            self.approval_tx
                .write()
                .insert(self.conversation_id.clone(), tx);
        }
    }

    /// 正常完成：usage 累加 + 替换 Streaming 占位 + 清理。
    fn on_done(&mut self, agent: &Agent<Static>) {
        if let Some(ref usage) = agent.agent.last_usage {
            accumulate_usage(self.runtimes, &self.conversation_id, usage);
        }
        {
            let mut all = self.runtimes.write();
            if let Some(rt) = all.get_mut(&self.conversation_id) {
                rt.request_count += 1;
            }
        }
        self.approval_tx.write().remove(&self.conversation_id);
        if let Some(segments) = self.segments.take() {
            let ui_msg = build_static_message(&segments);
            let mut all = self.runtimes.write();
            if let Some(rt) = all.get_mut(&self.conversation_id) {
                if let Some(pos) = rt
                    .messages
                    .iter()
                    .position(|m| matches!(m, UiMessage::Streaming { .. }))
                {
                    rt.messages[pos] = ui_msg;
                }
            }
        }
        self.is_streaming.set(false);
        self.streaming_project_id
            .write()
            .retain(|id| id != &self.project_id);
        self.streaming_states
            .lock()
            .expect("streaming_states lock poisoned")
            .remove(&self.conversation_id);
    }

    /// 取消/错误中止（agent 壳已随变换消费丢失）：移除占位、清理审批通道、上报错误。
    fn on_interrupt(&mut self, interrupt: &InterruptState) {
        {
            let mut all = self.runtimes.write();
            if let Some(rt) = all.get_mut(&self.conversation_id) {
                rt.messages.retain(|m| !matches!(m, UiMessage::Streaming { .. }));
            }
        }
        self.approval_tx.write().remove(&self.conversation_id);
        match interrupt {
            InterruptState::Cancelled => {
                tracing::info!(target: "agent", conversation_id = %self.conversation_id, "agent loop cancelled");
            }
            InterruptState::Error(e) => {
                self.error_signal.write().push(
                    ErrorInfo::new(
                        "AGENT_ERROR",
                        "Agent 执行失败",
                        format!("{}", e),
                        ErrorSeverity::Warning,
                        ErrorSource::Agent,
                    )
                    .with_detail(format!("{:#}", e)),
                );
            }
        }
        self.is_streaming.set(false);
        self.streaming_project_id
            .write()
            .retain(|id| id != &self.project_id);
        self.streaming_states
            .lock()
            .expect("streaming_states lock poisoned")
            .remove(&self.conversation_id);
    }
}

pub async fn run_agent_loop(ctx: BridgeContext) {
    let BridgeContext {
        user_input,
        action_mode,
        agent_mode: agent_mode_val,
        mut runtimes,
        mut is_streaming,
        streaming_project_id,
        project_id,
        cancel_token,
        conversation_id,
        streaming_states,
        mut error_signal,
        approval_tx,
    } = ctx;

    is_streaming.set(true);
    // 确保 agent 在缓存（可能从 DB 重建）
    if let Err(e) = AgentManager::get().init(&conversation_id) {
        error_signal.write().push(
            ErrorInfo::new(
                "AGENT_ERROR",
                "Agent init fail",
                format!("{}", e),
                ErrorSeverity::Warning,
                ErrorSource::Agent,
            )
            .with_detail(format!("{:#}", e)),
        );
        return;
    }

    // 从 Manager 取出 AgentCore，包成状态机配置态 Agent<Static>（所有权交给 AgentContext）
    let core = AgentManager::get()
        .remove(&conversation_id)
        .expect("agent must be in cache before run_agent_loop");
    let agent = Agent {
        running: Static,
        agent: core,
    };
    // handler 从前端 runtime 拿（前端 ConversationRuntime.agent_handler 是持有者，
    // plan_signal/approve_plan 与运行中的 agent 共享同一句柄）；无则生成默认并写回
    let handler = {
        let mut all = runtimes.write();
        let rt = all.entry(conversation_id.clone()).or_default();
        rt.agent_handler.clone().unwrap_or_else(|| {
            let h = AgentHandler::default();
            rt.agent_handler = Some(h.clone());
            h
        })
    };
    handler.set_action_mode(action_mode);
    *handler.agent_mode.lock().expect("agent_mode lock poisoned") = agent_mode_val;

    // 从 DB 恢复累计 token 用量（切换回已有对话时恢复之前的数据）
    let db_usage = crate::db::with_db(|conn| {
        crate::db::metadata::conversation::get_usage(conn, &conversation_id).unwrap_or_default()
    });
    let db_requests = crate::db::with_db(|conn| {
        crate::db::metadata::conversation::get_request_count(conn, &conversation_id).unwrap_or(0)
    });

    // 用户输入 → LLM 消息，进入流式阶段（内部创建 streaming_handle）
    let input = vec![llm::Message {
        role: llm::Role::User,
        content: Some(user_input),
        timestamp: Some(chrono::Utc::now()),
        ..Default::default()
    }];

    let ui = UiContext {
        runtimes,
        is_streaming,
        streaming_project_id,
        project_id,
        conversation_id: conversation_id.clone(),
        streaming_states,
        error_signal,
        approval_tx,
        segments: None,
        db_usage,
        db_requests,
    };
    // 事件 → tick（事件消费 future 与状态变换同任务 select 并发，流式输出实时刷新）
    let on_event = {
        let mut runtimes = ui.runtimes;
        let cid = ui.conversation_id.clone();
        move |_ev: &crate::state_agent::hook::AgentEvent| {
            runtimes.write().entry(cid.clone()).or_default().tick += 1;
        }
    };

    // AgentContext 白盒驱动：UiContext 在每个状态变换点直接做 UI 收尾
    let mut ctx = AgentContext::new(agent, ui);
    let result = ctx
        .run(
            input,
            cancel_token,
            handler,
            // 主对话（交互）：Ask 一律走审批管道（UI 审批卡注入）
            Box::new(crate::state_agent::ApprovalChain::new(vec![
                Box::new(crate::state_agent::UserApprovalGate),
            ])),
            on_event,
        )
        .await;

    // 统一缓存处理（一处集中，不散落）
    match result {
        Ok(()) => {
            if let Some(a) = ctx.take_agent() {
                AgentManager::get().register(a.agent);
            }
        }
        Err(_) => {
            // Err 时 Agent 壳已随变换消费丢失，消息均已落库，从 DB 重建并注册回缓存
            let _ = AgentManager::get().init(&conversation_id);
        }
    }
}

/// 把一次 LLM 交互的 token 用量累加到 conversation runtime（UI 看板展示）。
fn accumulate_usage(
    mut runtimes: Signal<std::collections::HashMap<String, crate::ui::state::ConversationRuntime>>,
    conversation_id: &str,
    usage: &TokenUsageRecord,
) {
    let mut all = runtimes.write();
    if let Some(rt) = all.get_mut(conversation_id) {
        rt.accumulated_usage.prompt_tokens += usage.prompt_tokens;
        rt.accumulated_usage.completion_tokens += usage.completion_tokens;
        rt.accumulated_usage.reasoning_tokens += usage.reasoning_tokens;
        rt.accumulated_usage.total_tokens += usage.total_tokens;
        rt.accumulated_usage.cache_hit_tokens += usage.cache_hit_tokens;
        rt.accumulated_usage.cache_miss_tokens += usage.cache_miss_tokens;
        rt.last_loop_usage = Some(usage.clone());
    }
}

/// 从流式 segments 快照构建静态消息（替换 Streaming 占位）。
fn build_static_message(segments: &Arc<Mutex<Vec<StreamSegment>>>) -> UiMessage {
    let snapshot = segments.lock().expect("segments lock poisoned").clone();
    let content = snapshot
        .iter()
        .filter_map(|s| match s {
            StreamSegment::Text(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    let reasoning = snapshot
        .iter()
        .filter_map(|s| match s {
            StreamSegment::Reasoning(t) => Some(t.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("");
    UiMessage::Static(ChatMessage {
        role: crate::model::Role::Assistant,
        content,
        timestamp: chrono::Local::now(),
        reasoning,
        segments: snapshot,
        content_html: String::new(),
    })
}
