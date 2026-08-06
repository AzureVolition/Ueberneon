// ── 通用宿主：AgentContext + PhaseObserver ─────────────────────────────────
//
// 设计动机：`Agent<Running<Streaming>>` / `Agent<Running<Executing>>` 是不同类型，
// 状态变换消费 self，外部难以整体持有"运行中对象"（旁路回调只能拿片段参数）。
// AgentContext 拥有 `Agent<Static>` 所有权并内联驱动循环：每次状态变换时持有
// 完整 `Agent<...>` 对象，交给 PhaseObserver 处理——UI 收尾可直接访问
// `agent.running.streaming_handle.segments`、`agent.agent.last_usage`、
// `agent.running.approval_tx` 等 pub 字段，不再需要参数打包传递。

use tokio_util::sync::CancellationToken;

use llm::Message;

use super::hook::AgentEvent;
use super::{
    Agent, AgentHandler, ApprovalGate, Executing, InterruptState, Running, Static, StreamResult,
    Streaming,
};

/// 阶段观察者：在 AgentContext 白盒驱动的每个状态变换点被调用。
///
/// 与"片段回调"不同，各方法接收**完整运行对象**引用——UI 收尾可直达
/// `agent.running.streaming_handle.segments`、`agent.agent.last_usage`、
/// `agent.running.approval_tx` 等 pub 字段。所有方法默认空实现；`()` 为 noop
/// （便捷路径 `Agent<Static>::run()` 使用）。
pub trait PhaseObserver {
    /// accept_message 成功（及每轮工具执行后续跑前）进入流式态。
    /// 可访问 `agent.running.streaming_handle.segments`（建 Streaming 占位）。
    fn on_streaming(&mut self, _agent: &Agent<Running<Streaming>>) {}
    /// 有工具待执行（Continue 后、execute 前）：本轮 usage 已写入、approval_tx 已设置。
    /// 可读 `agent.agent.last_usage`（usage 累加）、clone `agent.running.approval_tx`（审批注入）。
    fn on_executing(&mut self, _agent: &Agent<Running<Executing>>) {}
    /// 正常完成（回到配置态）：可读 `agent.agent.last_usage`（占位替换等收尾）。
    fn on_done(&mut self, _agent: &Agent<Static>) {}
    /// 取消/错误中止：agent 壳已随变换消费丢失（调用方从 DB 重建），仅收尾。
    fn on_interrupt(&mut self, _interrupt: &InterruptState) {}
}

/// noop 观察者：`Agent<Static>::run()`（不带 observer 的便捷路径）使用。
impl PhaseObserver for () {}

/// 通用宿主：拥有 `Agent<Static>` 所有权，白盒驱动状态机直至完成/中止。
///
/// - `agent`：空闲/完成时为 `Some`，运行中为 `None`（对象在循环中作为局部变量流动）。
/// - `run()` 返回 `Ok(())` 时 agent 已存回 `self.agent`（`take_agent` 可取）；
///   返回 `Err(interrupt)` 时壳已随变换消费丢失。
pub struct AgentContext<O> {
    pub agent: Option<Agent<Static>>,
    pub observer: O,
}

impl<O: PhaseObserver> AgentContext<O> {
    pub fn new(agent: Agent<Static>, observer: O) -> Self {
        Self {
            agent: Some(agent),
            observer,
        }
    }

    /// 取回 agent 所有权（run 结束后）。
    pub fn take_agent(&mut self) -> Option<Agent<Static>> {
        self.agent.take()
    }

    /// 完整 agent 循环（白盒）：stream ↔ execute 交替直至 Done 或 InterruptState。
    ///
    /// - `on_event`：AgentEvent 逐条转发（UI 据此 tick 重渲染）。事件消费 future 与
    ///   状态变换**同任务 select 并发**——不用 tokio::spawn，因为回调要写 Dioxus
    ///   Signal（`UnsyncStorage` 非 Send），必须留在调用方任务内；变换 await 期间事件
    ///   到达即回调（实时刷新），积压事件批量 drain。
    /// - 成功：`on_done` 后 agent 存回 `self.agent`，返回 `Ok(())`。
    /// - 中止：`on_interrupt` 后返回 `Err(interrupt)`（取消静默 / 错误由调用方上报）。
    pub async fn run<G>(
        &mut self,
        input: Vec<Message>,
        cancel_token: CancellationToken,
        handler: AgentHandler,
        approval_gate: Box<dyn ApprovalGate>,
        on_event: G,
    ) -> Result<(), InterruptState>
    where
        G: FnMut(&AgentEvent),
    {
        let agent = self.agent.take().expect("agent must be set before run");
        let (mut running, mut rx) = match agent
            .accept_message(input, cancel_token.clone(), handler, approval_gate)
            .await
        {
            Ok(v) => v,
            Err(interrupt) => {
                // accept_message 失败（request_stream/落库错误）：同样通知观察者收尾
                // （is_streaming 复位、错误上报等），否则 UI 会停留在流式状态
                self.observer.on_interrupt(&interrupt);
                return Err(Self::handle_interrupt(interrupt));
            }
        };
        self.observer.on_streaming(&running);

        // 事件消费 future：与状态变换同任务并发（select），事件到达即回调 on_event。
        // rx 随 running 被消费/丢弃而关闭（events sender drop），此后本 future 完成。
        let event_fut = async {
            let mut on_event = on_event;
            while let Some(ev) = rx.recv().await {
                on_event(&ev);
                // 批量 drain：一次消费全部堆积事件，减少 select 轮询次数
                while let Ok(ev) = rx.try_recv() {
                    on_event(&ev);
                }
            }
        };
        tokio::pin!(event_fut);

        let result = loop {
            let mut stream_fut = Box::pin(running.stream_message());
            let stream_result = tokio::select! {
                biased;
                r = &mut stream_fut => r,
                // 事件 future 只在 rx 关闭（running 已 drop）时完成，循环中不可达
                _ = &mut event_fut => unreachable!("event future finished while running is alive"),
            };
            match stream_result {
                Ok(StreamResult::Done(agent)) => {
                    self.observer.on_done(&agent);
                    self.agent = Some(agent);
                    break Ok(());
                }
                Ok(StreamResult::Continue(executing)) => {
                    self.observer.on_executing(&executing);
                    let mut exec_fut = Box::pin(executing.execute());
                    let exec_result = tokio::select! {
                        biased;
                        r = &mut exec_fut => r,
                        _ = &mut event_fut => unreachable!("event future finished while running is alive"),
                    };
                    match exec_result {
                        Ok(next) => {
                            // 续跑下一轮流式（Running → Running）：同一事件通道 /
                            // streaming_handle 经 into_phase 原样传递，UI 绑定的 segments
                            // Arc 不变；工具结果已在 messages 中，无需新输入。
                            self.observer.on_streaming(&next);
                            running = next;
                        }
                        Err(interrupt) => {
                            self.observer.on_interrupt(&interrupt);
                            break Err(Self::handle_interrupt(interrupt));
                        }
                    }
                }
                Err(interrupt) => {
                    self.observer.on_interrupt(&interrupt);
                    break Err(Self::handle_interrupt(interrupt));
                }
            }
        };
        // 收尾：等事件消费 future 结束（Done/Err 时 events sender 必已 drop → rx 关闭，
        // 积压事件全部处理完）；同任务内 await，无 Send 要求。
        let _ = (&mut event_fut).await;
        result
    }

    /// InterruptState 单独处理：取消与异常分别收尾（仅日志；事件回调已移入子任务，
    /// Stop 事件无运行时消费者，返回值本身即收尾信号）。
    fn handle_interrupt(interrupt: InterruptState) -> InterruptState {
        match &interrupt {
            InterruptState::Cancelled => {
                tracing::info!(target: "agent", "agent loop cancelled");
            }
            InterruptState::Error(e) => {
                tracing::error!(target: "agent", error = %e, "agent loop aborted");
            }
        }
        interrupt
    }
}
