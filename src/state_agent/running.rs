// ── 状态类型 Agent 运行态（running.rs） ────────────────────────────────────
//
// Agent<T> 状态机的运行阶段定义与状态变换：
//
//   Agent<Static> ──accept_message──▶ Agent<Running<Streaming>>
//   Agent<Running<Streaming>> ──stream_message──▶ StreamResult
//     Done(Agent<Static>) | Continue(Agent<Running<Executing>>)
//   Agent<Running<Executing>> ──execute──▶ Agent<Running<Streaming>>  （续跑下一轮流式）
//
// 工具循环是 Running → Running 变换：streaming_handle / 事件通道经 into_phase
// 原样传递，UI 绑定的 segments Arc 全程不变；只有无工具时回到 Agent<Static>。
// 每个变换返回 `Result<Agent<Next>, InterruptState>`：Err(Cancelled) 用户取消、
// Err(Error) 异常中止，由 mod.rs 的 `Agent<Static>::run()` 分别收尾。
//
// 多工具调用：LLM 一次返回的多个 tool_call 在流式阶段**全部** push 进 segments
// （前端一次性可见），execute 阶段按顺序从第一个开始执行。审批走
// `mpsc::Sender<(String /* tool_call_id */, bool /* approved */)>` 管道——
// 子线程收管道改 record 状态（allow → Pending 待执行，deny → Denied），
// 主线程按 record 状态顺序执行（详见 Executing 注释）。

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use llm::provider::ChunkStream;
use llm::{Chunk, Message, Role as LlmRole, ToolCall, Usage};

use super::approval::ApprovalGate;
use super::hook::{AgentEvent, DeltaKind};
use super::{Agent, AgentHandler, AgentMode, InterruptState, Static, ToolContext, ToolResult};
use crate::model::{
    StreamSegment, StreamingState, TokenUsageRecord, ToolCallRecord, ToolCallStatus,
};
use crate::permission::Decision;
use crate::tools::internal::common::checkable_tool::CheckableTool;

// ── select! 辅助枚举 ────────────────────────────────────────────────────────

enum StreamOrCancel<T> {
    Chunk(T),
    Cancelled,
}

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

/// 流式阶段的结果：要么正常完成（带回配置态 Agent），要么有工具待执行。
pub enum StreamResult {
    /// 无工具调用，正常完成
    Done(Agent<Static>),
    /// 有工具待执行（agent.running.approval_tx 为审批注入通道，驱动者可取出交给 UI）
    Continue(Agent<Running<Executing>>),
}

// ── Running：单轮运行上下文（跨 Streaming / Executing 阶段存活） ──────────

pub struct Running<S> {
    /// 当前阶段：Streaming（流式接收）或 Executing（执行工具）
    pub streaming: S,
    /// 运行时控制句柄（action_mode / agent_mode / current_plan），随状态变换流转
    pub handler: AgentHandler,
    /// 审批策略链（Ask 后如何决策，随 Running 状态变换流转）
    pub approval_gate: Box<dyn ApprovalGate>,
    /// 共享流式状态（segments + 审批通道），跨阶段存活，UI 经 Arc 读取
    pub streaming_handle: StreamingState,
    /// 循环数（LLM 请求次数）
    pub round: u32,
    /// 是否已停止（取消/出错）
    pub stopped: bool,
    /// 当前运行状态（UI 可见）
    pub state: AgentState,
    /// 取消令牌
    pub cancel_token: CancellationToken,
    /// 审批注入通道发送端：Some(tx) 时 UI 用 `tx.try_send((tool_call_id, approved))`
    /// 回传审批结果；None 时 Ask 自动拒绝（如非交互场景）
    pub approval_tx: Option<mpsc::Sender<(String, bool)>>,
    /// 事件通道（执行节点 emit，UI/调用方订阅）
    events: mpsc::UnboundedSender<AgentEvent>,
    /// 运行时 hook 注册表（跨阶段传递；stall hooks 由 plan 生命周期动态注册/注销）
    pub hook_register: super::HookRegister,
}

impl<S> Running<S> {
    /// 向事件通道投递事件（unbounded：不阻塞、不丢；无订阅者时静默丢弃）
    pub fn emit(&self, event: AgentEvent) {
        let _ = self.events.send(event);
    }

    /// 更新运行状态并广播 StateChanged 事件
    pub fn set_state(&mut self, state: AgentState) {
        self.state = state;
        self.emit(AgentEvent::StateChanged { state });
    }

    /// 阶段迁移：替换 streaming 载荷，其余单轮状态原样保留
    pub fn into_phase<S2>(self, streaming: S2) -> Running<S2> {
        Running {
            streaming,
            handler: self.handler,
            approval_gate: self.approval_gate,
            streaming_handle: self.streaming_handle,
            round: self.round,
            stopped: self.stopped,
            state: self.state,
            cancel_token: self.cancel_token,
            approval_tx: self.approval_tx,
            events: self.events,
            hook_register: self.hook_register,
        }
    }
}

impl Running<Streaming> {
    /// 从已建立的流式请求创建运行态，返回 (Run, 事件接收端)。
    pub fn init(
        streaming: Streaming,
        cancel_token: CancellationToken,
        handler: AgentHandler,
        approval_gate: Box<dyn ApprovalGate>,
    ) -> (Self, mpsc::UnboundedReceiver<AgentEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let run = Self {
            streaming,
            handler,
            approval_gate,
            streaming_handle: StreamingState {
                segments: Arc::new(Mutex::new(Vec::new())),
            },
            round: 0,
            stopped: false,
            state: AgentState::Idle,
            cancel_token,
            approval_tx: None,
            events: tx,
            // 空注册表：stall hooks 由 plan 生命周期动态注册（CreatePlan 后），
            // 非 plan 对话不注册（见 register_stall_hooks）
            hook_register: super::HookRegister::new(),
        };
        (run, rx)
    }
}

// ── Streaming：流式接收 LLM 输出 ───────────────────────────────────────────

pub struct Streaming {
    pub stream: ChunkStream,
    pub content: String,
    pub reason_content: String,
    /// 本轮收集的 tool_call id 列表（顺序 = 执行顺序，从第一个开始）
    pub tool_id_list: Vec<String>,
}

impl Streaming {
    pub fn init(stream: ChunkStream) -> Self {
        Self {
            stream,
            content: String::new(),
            reason_content: String::new(),
            tool_id_list: Vec::new(),
        }
    }
}

// ── Executing：按顺序执行工具，Ask 走 (tool_call_id, bool) 审批管道 ────────
//
// 子线程写状态、主线程等待：
// - 主线程 spawn 一个后台任务，专收审批管道 (tool_call_id, approved)
//   · 收到决策先写 segments 里 record 状态（allow → Pending 待执行，deny → Denied），
//     再广播 ToolCallDecision 事件（UI 瞬时刷新审批卡）+ Notify（唤醒主线程重读）
//   · 通道关闭（所有 Sender drop）→ 置 closed 标记 + Notify 唤醒，主线程重读时自动拒绝
// - 主线程批量 pre_check 后，按 record 状态顺序执行：前端显示靠 segments 里的
//   record（AwaitingApproval 渲染审批卡）；`PendingApproval` 仅作为审批挂起点
//   的数据结构（ApprovalGate 的 Session 载荷），不参与 UI 通知

pub struct Executing {
    /// 待执行的 tool_call id（顺序执行）
    pub tool_id_list: Vec<String>,
    /// 审批结果管道接收端（take 后 move 进子线程）
    approval_rx: Option<mpsc::Receiver<(String, bool)>>,
}

impl Executing {
    pub fn init(tool_id_list: Vec<String>) -> (Self, mpsc::Sender<(String, bool)>) {
        let (tx, rx) = mpsc::channel(32);
        (
            Self {
                tool_id_list,
                approval_rx: Some(rx),
            },
            tx,
        )
    }
}

// ── Agent 通用：事件 / 状态 / 归还配置态 ───────────────────────────────────

impl<S> Agent<Running<S>> {
    /// 向事件通道投递事件
    pub fn emit(&self, event: AgentEvent) {
        self.running.emit(event);
    }

    /// 更新运行状态并广播
    pub fn set_state(&mut self, state: AgentState) {
        self.running.set_state(state);
    }

    /// 归还 Agent 所有权（丢弃本轮运行态，回到配置态）
    pub fn into_agent(self) -> Agent<Static> {
        Agent {
            running: Static,
            agent: self.agent,
        }
    }
}

// ── 状态变换：Streaming → (无工具: 完成 | 有工具: Executing) ───────────────

impl Agent<Running<Streaming>> {
    /// 消费流式输出：文本/推理 push 到 segments，tool_call 全部收集（前端可见）。
    ///
    /// - `Ok(StreamResult::Done)`：无工具调用，正常完成，回到配置态
    /// - `Ok(StreamResult::Continue)`：有工具待执行，进入 Executing 阶段
    /// - `Err(InterruptState::Cancelled)`：用户取消
    /// - `Err(InterruptState::Error(msg))`：流式异常
    pub async fn stream_message(mut self) -> Result<StreamResult, InterruptState> {
        self.running.round += 1;
        self.running.set_state(AgentState::Streaming);
        // 每轮独立用量：先清空跨轮残留，避免上一轮无 Usage chunk 时重复上报
        self.agent.last_usage = None;

        let segments = self.running.streaming_handle.segments.clone();
        let events = self.running.events.clone();
        let cancel = self.running.cancel_token.clone();

        let mut output = String::new();
        let mut reasoning = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut last_usage: Option<Usage> = None;
        let mut abort: Option<InterruptState> = None;

        // 流式循环：只做字段级借用 + 局部 Arc/sender，避免整体 &mut self 与 stream 借用冲突
        {
            let stream = &mut self.running.streaming.stream;
            tokio::pin!(stream);
            loop {
                let result = tokio::select! {
                    _ = cancel.cancelled() => StreamOrCancel::Cancelled,
                    r = stream.next() => StreamOrCancel::Chunk(r),
                };
                match result {
                    StreamOrCancel::Cancelled => {
                        // 记中止标记，循环后统一把已收内容入史再退出
                        abort = Some(InterruptState::Cancelled);
                        break;
                    }
                    StreamOrCancel::Chunk(None) => break,
                    StreamOrCancel::Chunk(Some(Ok(Chunk::Text(t)))) => {
                        output.push_str(&t);
                        self.running.streaming.content.push_str(&t);
                        push_text(&segments, &t);
                        let _ = events.send(AgentEvent::StreamDelta {
                            kind: DeltaKind::Text,
                        });
                    }
                    StreamOrCancel::Chunk(Some(Ok(Chunk::Reasoning { text, .. }))) => {
                        reasoning.push_str(&text);
                        self.running.streaming.reason_content.push_str(&text);
                        push_reasoning(&segments, &text);
                        let _ = events.send(AgentEvent::StreamDelta {
                            kind: DeltaKind::Reasoning,
                        });
                    }
                    StreamOrCancel::Chunk(Some(Ok(Chunk::ToolCallComplete(tc)))) => {
                        // 全部收集：tool_calls 入史、tool_id_list 供顺序执行、record push 前端
                        tool_calls.push(tc.clone());
                        self.running.streaming.tool_id_list.push(tc.id.clone());
                        let rec = ToolCallRecord {
                            id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                            result: None,
                            status: ToolCallStatus::Pending,
                            approval_reason: None,
                        };
                        {
                            let mut segs = segments.lock().expect("segments lock poisoned");
                            segs.push(StreamSegment::ToolCall(rec));
                        }
                        // 发事件：纯工具响应（无文本）也要触发 UI 刷新，审批卡及时显示
                        let _ = events.send(AgentEvent::ToolCallQueued {
                            tool_name: tc.name.clone(),
                            args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                        });
                    }
                    StreamOrCancel::Chunk(Some(Ok(Chunk::Usage(usage)))) => {
                        last_usage = Some(match &last_usage {
                            Some(last) => Usage {
                                prompt_tokens: last.prompt_tokens + usage.prompt_tokens,
                                completion_tokens: last.completion_tokens + usage.completion_tokens,
                                reasoning_tokens: last.reasoning_tokens + usage.reasoning_tokens,
                                total_tokens: last.total_tokens + usage.total_tokens,
                                cache_hit_tokens: last.cache_hit_tokens + usage.cache_hit_tokens,
                                cache_miss_tokens: last.cache_miss_tokens + usage.cache_miss_tokens,
                                finish_reason: format!(
                                    "{}\n{}",
                                    last.finish_reason, usage.finish_reason
                                ),
                            },
                            None => usage,
                        });
                    }
                    StreamOrCancel::Chunk(Some(Ok(_))) => {} // Start / Delta
                    StreamOrCancel::Chunk(Some(Err(e))) => {
                        let msg = format!("Stream error: {e}");
                        output.push_str(&msg);
                        push_text(&segments, &msg);
                        let _ = events.send(AgentEvent::Error {
                            message: msg.clone(),
                        });
                        abort = Some(InterruptState::Error(msg));
                        break;
                    }
                }
            }
        }

        // 流式结束（正常或提前终止均把已收内容入史 + 落库，复用 AgentCore::push_message）
        let assistant_msg = Message {
            role: LlmRole::Assistant,
            content: if output.is_empty() {
                None
            } else {
                Some(output.clone())
            },
            reasoning_content: if reasoning.is_empty() {
                None
            } else {
                Some(reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
            timestamp: Some(chrono::Utc::now()),
            ..Default::default()
        };
        // content / reasoning_content / tool_calls 全空（如流式中断未收到任何内容）→
        // 无意义消息：不落库也不入内存历史，避免空消息混入后续请求历史。
        // tool_calls 非空的工具调用回合即使无 content/reasoning 也必须保留（tool 配对）。
        let is_empty_assistant = assistant_msg.content.as_deref().map_or(true, str::is_empty)
            && assistant_msg
                .reasoning_content
                .as_deref()
                .map_or(true, str::is_empty)
            && assistant_msg.tool_calls.is_empty();
        if !is_empty_assistant {
            self.agent
                .push_message(assistant_msg)
                .map_err(|e| InterruptState::Error(format!("save message: {e}")))?;
        }

        // token 用量持久化（复用 AgentCore::persist_usage）
        if let Some(u) = last_usage {
            let record = TokenUsageRecord {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                reasoning_tokens: u.reasoning_tokens,
                total_tokens: u.total_tokens,
                cache_hit_tokens: u.cache_hit_tokens,
                cache_miss_tokens: u.cache_miss_tokens,
            };
            self.agent.persist_usage(&record);
            // 写入跨轮核心，收尾时（Done/Err）仍可读取
            self.agent.last_usage = Some(record);
        }

        // 中止：状态置位 + 广播后返回 InterruptState（已收内容已入史落库）
        if let Some(interrupt) = abort {
            match &interrupt {
                InterruptState::Cancelled => self.running.state = AgentState::Cancelled,
                InterruptState::Error(_) => self.running.state = AgentState::Error,
            }
            self.emit(AgentEvent::StateChanged {
                state: self.running.state,
            });
            return Err(interrupt);
        }

        // 无工具调用 → 正常完成（回到配置态）
        if self.running.streaming.tool_id_list.is_empty() {
            let agent = self.agent;
            return Ok(StreamResult::Done(Agent {
                running: Static,
                agent,
            }));
        }

        // 有工具 → 进入 Executing 阶段，审批管道发送端交给驱动者/UI
        let tool_id_list = std::mem::take(&mut self.running.streaming.tool_id_list);
        let (executing, tx) = Executing::init(tool_id_list);
        self.running.approval_tx = Some(tx);
        let agent_core = self.agent;
        let running = self.running.into_phase(executing);
        Ok(StreamResult::Continue(Agent {
            running,
            agent: agent_core,
        }))
    }
}

// ── 状态变换：Executing → Static（执行全部工具） ────────────────────────────

impl Agent<Running<Executing>> {
    /// 按顺序执行本轮全部工具（从第一个开始）。
    ///
    /// 子线程改状态、主线程等待：
    /// 1. spawn 后台任务专收审批管道 `(tool_call_id, approved)`——收到决策先写
    ///    segments 里 record 状态（allow → Pending 待执行，deny → Denied），
    ///    再广播 `ToolCallDecision` 事件（UI 瞬时刷新审批卡）+ `Notify`（唤醒主线程）。
    /// 2. 批量 pre_check：Ask 的工具再过 `approval_gate`（Running 内注入的策略链）决策——
    ///    Allow → 保持 Pending 直接执行；Deny → 置 Denied；Ask → 置
    ///    AwaitingApproval（前端同时显示多个审批卡，显示靠 segments record）。
    /// 3. 主循环按 record 状态顺序执行（状态驱动收敛循环）：Pending → 直接执行；
    ///    AwaitingApproval → 注册 Notify permit 等 record 变化（子线程写 record 后
    ///    广播，醒来重读收敛）；Denied → 拒绝落库。
    ///
    /// `approval_gate` 在构筑 Running 时注入：主对话用 `ApprovalChain([UserApprovalGate])`
    /// （Ask 走管道等 UI），子 Agent 用 `ApprovalChain([AutoDenyApprovalGate])`（自动拒绝）。
    /// 审批管道（mpsc (tool_call_id, approved)）由 execute 管理，gate 不参与。
    ///
    /// 工具执行完毕不回到配置态，而是直接续跑下一轮流式：返回
    /// `Agent<Running<Streaming>>`（Running → Running 变换，streaming_handle /
    /// 事件通道经 `into_phase` 原样传递，UI 绑定的 segments Arc 全程不变）。
    pub async fn execute(mut self) -> Result<Agent<Running<Streaming>>, InterruptState> {
        self.running.set_state(AgentState::Executing);
        // 轮开始事件送 hook_register（stall 计数 hook 监听 Executing；
        // events channel 的 StateChanged 走 Running::set_state，两者独立）
        self.running.hook_register.emit(&AgentEvent::StateChanged {
            state: AgentState::Executing,
        });
        let segments = self.running.streaming_handle.segments.clone();
        let cancel = self.running.cancel_token.clone();
        let plan_mode = self.running.handler.action_mode();
        let handler = self.running.handler.clone();
        let main_conversation_id = self.agent.conversation_id.clone();
        let project_id = self.agent.project_id.clone();

        // ── 子线程：收审批管道，写 record + 广播事件 + Notify 唤醒等待者 ──
        let mut approval_rx = self
            .running
            .streaming
            .approval_rx
            .take()
            .expect("approval_rx already taken");
        // 审批通道 sender 已交给外部（on_approval_tx 克隆），running 持有的 sender
        // 立即清空：否则 execute 阻塞在审批等待时该 sender 无法 drop，子线程
        // recv() 永不返回 None，UI 消失（drop 克隆）也触发不了自动拒绝 → 死锁
        self.running.approval_tx = None;
        // 共享通知：子线程写 record 后广播，主线程等 record 变化（代替 oneshot 槽配对）
        let notify = Arc::new(tokio::sync::Notify::new());
        let notify_thread = notify.clone();
        let segs_thread = segments.clone();
        let events_thread = self.running.events.clone();
        let thread_cancel = cancel.clone();
        // 审批通道是否已关闭（所有 Sender 都已 drop）：Ask 等待前检查，避免无人注入决策导致挂起
        let channel_closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let closed_thread = channel_closed.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = thread_cancel.cancelled() => break,
                    r = approval_rx.recv() => {
                        let Some((tool_call_id, approved)) = r else {
                            // 所有审批通道已关闭（非交互/UI 消失）：标记关闭 +
                            // 唤醒等待者（它们重读 record + closed 检查 → 自动拒绝）
                            closed_thread.store(true, std::sync::atomic::Ordering::SeqCst);
                            notify_thread.notify_waiters();
                            break;
                        };
                        // 决策统一落地：写 record（allow → Pending / deny → Denied，
                        // guard 未终态），再广播事件（UI 瞬时刷新审批卡）与 Notify
                        // （唤醒主线程重读收敛）。record 先于 notify 写，配合主线程
                        // "先注册 permit 再重读"，通知不会丢失。
                        set_record_decision(&segs_thread, &tool_call_id, approved);
                        let _ = events_thread.send(AgentEvent::ToolCallDecision {
                            call_id: tool_call_id.clone(),
                            approved,
                        });
                        notify_thread.notify_waiters();
                    }
                }
            }
        });

        let tool_count = self.running.streaming.tool_id_list.len();

        // ── 阶段一：批量 pre_check，Ask 的全部置 AwaitingApproval（前端同时显示审批卡） ──
        // 放飞自我（Unrestrained）：Ask 统一降级为 Allow（Deny 原样保留）。
        // approval_gate（UserApprovalGate 恒 Ask）与工具 pre_check 任一返回 Ask 都会在
        // combine 后体现，这里做最终兜底——一处覆盖全部审批来源，无需逐个工具分支。
        let agent_mode = *self
            .running
            .handler
            .agent_mode
            .lock()
            .expect("agent_mode lock poisoned");
        for i in 0..tool_count {
            if cancel.is_cancelled() {
                self.running.set_state(AgentState::Cancelled);
                return Err(InterruptState::Cancelled);
            }
            let call_id = self.running.streaming.tool_id_list[i].clone();
            let (tool_name, args) = self.record_info(&segments, &call_id)?;
            let Some(tool) = self.agent.registry.get(&tool_name) else {
                return Err(InterruptState::Error(format!(
                    "tool not registered: {tool_name}"
                )));
            };
            let ctx = ToolContext {
                call_id: call_id.clone(),
                plan_mode,
                handler: handler.clone(),
                progress: None,
                main_conversation_id: main_conversation_id.clone(),
                project_id: project_id.clone(),
                cancel_token: Some(cancel.clone()),
            };
            // gate 工具级门控（执行类工具 Ask / 只读放行）与工具自身 pre_check
            // （命令级危险判定：rm -rf、force push、系统路径等）合并，Deny > Ask > Allow
            let decision = self
                .running
                .approval_gate
                .decide(&tool_name, &args, tool.read_only())
                .combine(tool.pre_check(&ctx, &args));
            // 放飞自我：Ask → Allow；Deny（权限策略 / plan mode 拦截）不受影响
            let decision = apply_agent_mode(decision, agent_mode);
            match decision {
                Decision::Allow => {} // 保持 Pending，主循环直接执行
                Decision::Deny(msg) => {
                    self.set_record_status(
                        &segments,
                        &call_id,
                        ToolCallStatus::Denied(msg.clone()),
                    );
                }
                Decision::Ask => {
                    let reason = format!("{tool_name} needs approval");
                    self.set_record_status(
                        &segments,
                        &call_id,
                        ToolCallStatus::AwaitingApproval {
                            reason: reason.clone(),
                        },
                    );
                    self.emit(AgentEvent::ApprovalRequested {
                        tool_name,
                        args,
                        reason,
                    });
                }
            }
        }

        // ── 阶段二：按 record 状态顺序执行（子线程已把提前审批的工具改好状态） ──
        //
        // 每个工具用"状态驱动收敛循环"：循环顶部读 record 分派——
        //   Pending → 执行；Denied → 拒绝落库；AwaitingApproval → 注册 Notify permit
        //   等 record 变化（子线程写 record 后广播），醒来回顶部重读必然收敛，
        //   因此 Pending/Denied 分支各只有一处，无嵌套重复。
        for i in 0..tool_count {
            let call_id = self.running.streaming.tool_id_list[i].clone();
            let (tool_name, args) = self.record_info(&segments, &call_id)?;

            loop {
                if cancel.is_cancelled() {
                    self.running.set_state(AgentState::Cancelled);
                    return Err(InterruptState::Cancelled);
                }
                let status = record_status(&segments, &call_id).ok_or_else(|| {
                    InterruptState::Error(format!("tool call record not found: {call_id}"))
                })?;
                match status {
                    // 无需审批 或 已被批准：直接执行
                    ToolCallStatus::Pending => {
                        // 从等待收敛而来时 running.state 停在 WaitingApproval，复位为执行中
                        self.running.set_state(AgentState::Executing);
                        self.run_tool(&call_id, &tool_name, &args).await?;
                        break;
                    }
                    // pre_check 拒绝 或 用户拒绝：拒绝落库
                    ToolCallStatus::Denied(msg) => {
                        self.finalize_tool(&call_id, &tool_name, Err(msg), true)
                            .await?;
                        // 从等待收敛而来时 state 可能停在 WaitingApproval，复位为执行中
                        self.running.set_state(AgentState::Executing);
                        break;
                    }
                    // 等待审批：等子线程写 record + Notify 广播（取消则中止循环）
                    ToolCallStatus::AwaitingApproval { .. } => {
                        // 审批通道已关闭（无人会注入决策）→ 自动拒绝，不等
                        if channel_closed.load(std::sync::atomic::Ordering::SeqCst) {
                            self.finalize_tool(
                                &call_id,
                                &tool_name,
                                Err("approval channel closed".into()),
                                true,
                            )
                            .await?;
                            self.running.set_state(AgentState::Executing);
                            break;
                        }
                        self.running.set_state(AgentState::WaitingApproval);
                        // 先创建 notified future（tokio Notify 在创建时快照唤醒计数，
                        // 决策写 record 后必 notify_waiters，计数变化使该 future 首次
                        // poll 即完成——即使 notify 先于 await 到达也不会丢），
                        // 再重读 record：决策已落地 → 回顶部收敛；未落地 → await 等待。
                        let notified = notify.notified();
                        tokio::pin!(notified);
                        if !matches!(
                            record_status(&segments, &call_id).ok_or_else(|| {
                                InterruptState::Error(format!(
                                    "tool call record not found: {call_id}"
                                ))
                            })?,
                            ToolCallStatus::AwaitingApproval { .. }
                        ) {
                            continue;
                        }
                        // 注册后重查通道关闭：闭合"重读与注册 permit 之间子线程已关闭"的窗口
                        if channel_closed.load(std::sync::atomic::Ordering::SeqCst) {
                            self.finalize_tool(
                                &call_id,
                                &tool_name,
                                Err("approval channel closed".into()),
                                true,
                            )
                            .await?;
                            self.running.set_state(AgentState::Executing);
                            break;
                        }
                        tokio::select! {
                            _ = cancel.cancelled() => {
                                self.running.set_state(AgentState::Cancelled);
                                return Err(InterruptState::Cancelled);
                            }
                            _ = &mut notified => {}
                        }
                        continue; // 醒来重读 → 收敛（Pending/Denied，或 closed 检查 → 拒绝）
                    }
                    st => {
                        return Err(InterruptState::Error(format!(
                            "unexpected tool state {st:?} for {call_id}"
                        )));
                    }
                }
            }
        }

        // 本轮工具执行完毕：plan stall 计数 + 续跑下一轮流式。
        // Running → Running：streaming_handle / events / cancel_token / hook_register
        // 经 into_phase 原样传递，UI 绑定的 segments Arc 不变；工具结果已在 messages 中。
        // 取消时不再发起下一轮 LLM 请求（token 已取消，避免浪费一次 API 调用）
        if cancel.is_cancelled() {
            self.running.set_state(AgentState::Cancelled);
            return Err(InterruptState::Cancelled);
        }
        // hook 注入的提示词（如 stall 催促）收集进 messages（下一轮流式请求带上）
        for msg in self.running.hook_register.drain_prompts() {
            self.agent.messages.push(msg);
        }
        let stream = self.agent.request_stream().await?;
        let streaming = Streaming::init(stream);
        let running = self.running.into_phase(streaming);
        let agent = self.agent;
        Ok(Agent { running, agent })
    }

    /// 取 record 的工具名与参数（segments 内嵌 record 是唯一数据源）。
    fn record_info(
        &self,
        segments: &Arc<Mutex<Vec<StreamSegment>>>,
        call_id: &str,
    ) -> Result<(String, serde_json::Value), InterruptState> {
        let segs = segments.lock().expect("segments lock poisoned");
        let rec = segs.iter().rev().find_map(|s| match s {
            StreamSegment::ToolCall(r) if r.id == call_id => Some(r),
            _ => None,
        });
        match rec {
            Some(r) => Ok((r.tool_name.clone(), r.args.clone())),
            None => Err(InterruptState::Error(format!(
                "tool call record not found: {call_id}"
            ))),
        }
    }

    /// 执行一个已批准/无需审批的工具（状态置 Running + 事件 + 执行 + 落库）。
    async fn run_tool(
        &mut self,
        call_id: &str,
        tool_name: &str,
        args: &serde_json::Value,
    ) -> Result<(), InterruptState> {
        let Some(tool) = self.agent.registry.get(tool_name) else {
            return Err(InterruptState::Error(format!(
                "tool not registered: {tool_name}"
            )));
        };
        let segments = self.running.streaming_handle.segments.clone();
        let ctx = ToolContext {
            call_id: call_id.to_string(),
            plan_mode: self.running.handler.action_mode(),
            handler: self.running.handler.clone(),
            progress: None,
            main_conversation_id: self.agent.conversation_id.clone(),
            project_id: self.agent.project_id.clone(),
            cancel_token: Some(self.running.cancel_token.clone()),
        };
        self.set_record_status(&segments, call_id, ToolCallStatus::Running);
        self.emit(AgentEvent::ToolCallStart {
            tool_name: tool_name.into(),
            args: args.clone(),
        });
        let result = self.execute_with_cancel(&tool, &ctx, args).await;
        self.finalize_tool(call_id, tool_name, result, false).await
    }

    /// 更新 segments 中某条 tool record 的状态（及审批原因）。
    fn set_record_status(
        &self,
        segments: &Arc<Mutex<Vec<StreamSegment>>>,
        call_id: &str,
        status: ToolCallStatus,
    ) {
        let mut segs = segments.lock().expect("segments lock poisoned");
        if let Some(rec) = segs
            .iter_mut()
            .rev()
            .find(|s| matches!(s, StreamSegment::ToolCall(r) if r.id == call_id))
        {
            if let StreamSegment::ToolCall(rec) = rec {
                if let ToolCallStatus::AwaitingApproval { reason } = &status {
                    rec.approval_reason = Some(reason.clone());
                }
                rec.status = status;
            }
        }
    }

    /// 单工具执行（含取消拦截）。
    /// 返回工具执行结果（`Err(String)` 为工具自身的失败，由 finalize 落定状态）；
    /// 取消时返回 `Err("cancelled by user")` 并标记 stopped，由循环边界转成
    /// `InterruptState::Cancelled` 中止。
    async fn execute_with_cancel(
        &mut self,
        tool: &Arc<dyn CheckableTool + Send + Sync>,
        ctx: &ToolContext,
        args: &serde_json::Value,
    ) -> Result<ToolResult, String> {
        let cancel = self.running.cancel_token.clone();
        let exec = tool.execute(ctx, args);
        tokio::pin!(exec);
        tokio::select! {
            _ = cancel.cancelled() => {
                self.running.stopped = true;
                Err("cancelled by user".into())
            }
            r = &mut exec => r,
        }
    }

    /// 工具结果回写：record 状态 + 事件 + tool 消息入史落库（复用 AgentCore::push_message）。
    async fn finalize_tool(
        &mut self,
        call_id: &str,
        tool_name: &str,
        result: Result<ToolResult, String>,
        denied: bool,
    ) -> Result<(), InterruptState> {
        let segments = self.running.streaming_handle.segments.clone();
        {
            let mut segs = segments.lock().expect("segments lock poisoned");
            if let Some(rec) = segs
                .iter_mut()
                .rev()
                .find(|s| matches!(s, StreamSegment::ToolCall(r) if r.id == call_id))
            {
                if let StreamSegment::ToolCall(rec) = rec {
                    rec.result = Some(match &result {
                        Ok(tr) => tr.output.clone(),
                        Err(e) => e.clone(),
                    });
                    rec.status = match &result {
                        Ok(_) => ToolCallStatus::Success,
                        Err(e)
                            if denied
                                || e.starts_with("denied by user:")
                                || e == "approval channel closed" =>
                        {
                            ToolCallStatus::Denied(e.clone())
                        }
                        Err(e) => ToolCallStatus::Failed(e.clone()),
                    };
                }
            }
        }
        self.emit(AgentEvent::ToolCallEnd {
            tool_name: tool_name.into(),
            result: result.clone(),
        });
        self.running.hook_register.emit(&AgentEvent::PostToolUse {
            tool_name: tool_name.into(),
            result: result.clone(),
        });

        let tool_msg = Message {
            role: LlmRole::Tool,
            content: Some(match &result {
                Ok(tr) => tr.output.clone(),
                Err(e) => format!("error: {e}"),
            }),
            tool_call_id: Some(call_id.to_string()),
            tool_name: Some(tool_name.to_string()),
            timestamp: Some(chrono::Utc::now()),
            ..Default::default()
        };
        self.agent
            .push_message(tool_msg)
            .map_err(|e| InterruptState::Error(format!("save message: {e}")))?;

        // plan 生命周期驱动 stall hooks 注册/注销：CreatePlan 执行后
        // （current_plan 变 Some）注册；current_plan 清空（CompleteStep 完成等）注销。
        // 幂等，成本一次锁；注销同时撤回未注入的 stall 催促（见 unregister_stall_hooks）。
        let has_plan = self
            .running
            .handler
            .current_plan
            .lock()
            .expect("current_plan lock poisoned")
            .is_some();
        if has_plan {
            self.running
                .hook_register
                .register_stall_hooks(&self.running.handler);
        } else {
            self.running.hook_register.unregister_stall_hooks();
        }

        Ok(())
    }
}

// ── Arc 辅助操作 ─────────────────────────────────────────────────────────────

/// 子线程用：把提前审批的决策写进 record 状态
/// （allow → Pending 待执行，主线程看到直接执行；deny → Denied，拒绝落库）。
fn set_record_decision(segments: &Arc<Mutex<Vec<StreamSegment>>>, call_id: &str, approved: bool) {
    let mut segs = segments.lock().expect("segments lock poisoned");
    if let Some(rec) = segs
        .iter_mut()
        .rev()
        .find(|s| matches!(s, StreamSegment::ToolCall(r) if r.id == call_id))
    {
        if let StreamSegment::ToolCall(rec) = rec {
            // 只对未终态（等待审批/待执行）的 record 应用决策，
            // 避免迟到的/重复的审批覆盖已执行的终态结果
            if matches!(
                rec.status,
                ToolCallStatus::AwaitingApproval { .. } | ToolCallStatus::Pending
            ) {
                rec.status = if approved {
                    ToolCallStatus::Pending
                } else {
                    ToolCallStatus::Denied("denied by user".into())
                };
                rec.approval_reason = None;
            }
        }
    }
}

/// 读取某条 tool record 的当前状态（主线程执行前判定）。
fn record_status(
    segments: &Arc<Mutex<Vec<StreamSegment>>>,
    call_id: &str,
) -> Option<ToolCallStatus> {
    let segs = segments.lock().expect("segments lock poisoned");
    segs.iter().rev().find_map(|s| match s {
        StreamSegment::ToolCall(r) if r.id == call_id => Some(r.status.clone()),
        _ => None,
    })
}

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

/// 按 agent_mode 调整合并后的权限决策（pre_check 汇合点兜底）。
///
/// Unrestrained（放飞自我）：`Ask → Allow`（从不询问），`Deny` 原样保留——
/// 权限策略拒绝与 plan mode 写工具拦截不受影响；
/// 其余模式（Ask / Cautious / Auto）：决策原样透传。
///
/// 调用位置在 `approval_gate.decide().combine(tool.pre_check())` 之后，
/// 因此无论审批链（UserApprovalGate 恒 Ask）还是工具自身检查返回 Ask，
/// 放飞自我模式下都会在此统一放行，无需逐个工具分支。
fn apply_agent_mode(decision: Decision, agent_mode: AgentMode) -> Decision {
    match agent_mode {
        AgentMode::Unrestrained => match decision {
            Decision::Ask => Decision::Allow,
            other => other,
        },
        _ => decision,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 组合矩阵：gate 决策（Ask/Allow）+ 工具 pre_check 三种结果，
    /// combine 后经 apply_agent_mode(Unrestrained) 的最终决策。
    #[test]
    fn unrestrained_downgrades_ask_after_combine() {
        let gate = Decision::Ask; // UserApprovalGate 恒 Ask
        // gate Ask + pre_check Allow → combine Ask → 降级 Allow
        assert_eq!(
            apply_agent_mode(
                gate.clone().combine(Decision::Allow),
                AgentMode::Unrestrained
            ),
            Decision::Allow
        );
        // gate Ask + pre_check Ask → combine Ask → 降级 Allow
        assert_eq!(
            apply_agent_mode(gate.clone().combine(Decision::Ask), AgentMode::Unrestrained),
            Decision::Allow
        );
        // gate Ask + pre_check Deny → combine Deny（优先级 Deny > Ask）→ Deny 保留
        assert!(matches!(
            apply_agent_mode(
                gate.clone().combine(Decision::Deny("blocked".into())),
                AgentMode::Unrestrained
            ),
            Decision::Deny(_)
        ));
        // 双 Allow → Allow
        assert_eq!(
            apply_agent_mode(
                Decision::Allow.combine(Decision::Allow),
                AgentMode::Unrestrained
            ),
            Decision::Allow
        );
    }

    /// 其余模式（Ask / Cautious / Auto）：Ask 原样保留（审批照常），Deny 保留，Allow 透传。
    #[test]
    fn other_modes_keep_ask() {
        for mode in [AgentMode::Ask, AgentMode::Cautious, AgentMode::Auto] {
            assert_eq!(apply_agent_mode(Decision::Ask, mode), Decision::Ask);
            assert!(matches!(
                apply_agent_mode(Decision::Deny("x".into()), mode),
                Decision::Deny(_)
            ));
            assert_eq!(apply_agent_mode(Decision::Allow, mode), Decision::Allow);
        }
    }

    /// Unrestrained 下 Deny 不被吞（权限策略 / plan mode 拦截等仍生效）。
    #[test]
    fn unrestrained_keeps_deny() {
        assert!(matches!(
            apply_agent_mode(Decision::Deny("blocked".into()), AgentMode::Unrestrained),
            Decision::Deny(_)
        ));
        assert_eq!(
            apply_agent_mode(Decision::Allow, AgentMode::Unrestrained),
            Decision::Allow
        );
    }
}
