// AgentRun 驱动 → Dioxus signals 桥接层
//
// B2：不 spawn —— 直接持有 run.run_until_blocked() 的 future，在 select 里驱动。
// 职责：
// 1. 事件 → tick 触发 UI 重渲染
// 2. run 挂起（Blocked::Approval）→ 驱动者等用户点选，注入审批结果继续
// 3. run 结束（Blocked::Done）→ 替换 Streaming 为 Static + 收尾注册回缓存

use dioxus::prelude::*;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::{ActionMode, AgentMode, AgentRun, Blocked};
use crate::model::*;
use crate::permission::Decision;
use crate::ui::components::error::*;

/// bridge 运行时上下文 —— 打包 run_agent_loop 的所有入参。
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
}

pub async fn run_agent_loop(ctx: BridgeContext) {
    let BridgeContext {
        user_input,
        action_mode,
        agent_mode: agent_mode_val,
        mut runtimes,
        mut is_streaming,
        mut streaming_project_id,
        project_id,
        cancel_token,
        conversation_id,
        streaming_states,
        mut error_signal,
    } = ctx;

    is_streaming.set(true);
    if let Err(e) = crate::agent::manager::AgentManager::get()
        .init(&conversation_id){
            error_signal.write().push(ErrorInfo::new(
                "AGENT_ERROR",
                "Agent init fail",
                format!("{}", e),
                ErrorSeverity::Warning,
                ErrorSource::Agent,
            ).with_detail(format!("{:#}", e)));
            return;
        }
    // 从 Manager 取出 Agent，包成 AgentRun（执行上下文，方案 B：持有所有权）
    let agent = crate::agent::manager::AgentManager::get()
        .remove(&conversation_id)
        .expect("agent must be in cache before run_agent_loop");
    let (mut run, mut rx) = AgentRun::new(agent, cancel_token.clone());
    run.agent.handler.set_action_mode(action_mode);
    *run.agent.handler.agent_mode.lock().expect("agent_mode lock poisoned") = agent_mode_val;

    // 注入用户消息（进入 Streaming）
    if let Err(e) = run.begin(user_input) {
        error_signal.write().push(ErrorInfo::new(
            "AGENT_ERROR",
            "Agent begin fail",
            format!("{}", e),
            ErrorSeverity::Warning,
            ErrorSource::Agent,
        ).with_detail(format!("{:#}", e)));
        return;
    }

    // 从 DB 恢复累计 token 用量（切换回已有对话时恢复之前的数据）
    let db_usage = crate::db::with_db(|conn| {
        crate::db::metadata::conversation::get_usage(conn, &conversation_id)
            .unwrap_or_default()
    });
    let db_requests = crate::db::with_db(|conn| {
        crate::db::metadata::conversation::get_request_count(conn, &conversation_id)
            .unwrap_or(0)
    });

    // Run 内部创建流式状态
    let streaming = run.create_streaming();
    {
        let mut rts = runtimes.write();
        let rt = rts.entry(conversation_id.clone()).or_insert_with(|| crate::ui::state::ConversationRuntime {
            accumulated_usage: db_usage,
            request_count: db_requests,
            ..Default::default()
        });
        rt.messages.push(streaming.clone());
    }
    streaming_states.lock().expect("streaming_states lock poisoned").insert(conversation_id.clone(), streaming.clone());

    // 驱动：直接持有 run 的 future（不 spawn），审批挂起时由本循环接管
    let mut run_fut = Box::pin(run.run_until_blocked());
    let mut cancelled = false;
    let mut rx_closed = false;
    loop {
        tokio::select! {
            // biased：run_fut 总是先 poll —— 它 Ready 时必须当轮处理，
            // 否则遗留到下一轮再 poll 已完成的 future 会 panic（spawn task 静默死亡 → UI 卡死）
            biased;
            blocked = &mut run_fut => {
                let blocked = blocked;
                drop(run_fut); // 结束对 run 的借用，下面才能 &mut run

                match blocked {
                    Ok(Blocked::Approval(req, result_rx)) => {
                        // run 已让出。先消费事件通道并触发一次重渲染，确保审批卡显示：
                        // ApprovalRequested 事件在 run_fut Ready 前已入队，但 biased 下
                        // run_fut 优先、事件分支本轮未执行 —— 必须在这里补一次 tick。
                        while rx.try_recv().is_ok() {}
                        runtimes.write().entry(conversation_id.clone()).or_default().tick += 1;

                        // run 已让出：驱动者决定怎么等用户（可加超时/放弃）
                        let decision = tokio::select! {
                            r = result_rx => match r {
                                Ok(true) => Decision::Allow,
                                _ => Decision::Deny("denied by user".into()),
                            },
                            _ = cancel_token.cancelled() => Decision::Deny("cancelled by user".into()),
                        };
                        if let Err(e) = run.resolve_approval(&req, decision) {
                            // 移除 Streaming 占位 + 通知
                            {
                                let mut all = runtimes.write();
                                if let Some(rt) = all.get_mut(&conversation_id) {
                                    rt.messages.retain(|m| !matches!(m, UiMessage::Streaming { .. }));
                                }
                            }
                            error_signal.write().push(ErrorInfo::new(
                                "AGENT_ERROR",
                                "Approval resolve fail",
                                format!("{}", e),
                                ErrorSeverity::Warning,
                                ErrorSource::Agent,
                            ).with_detail(format!("{:#}", e)));
                            is_streaming.set(false);
                            streaming_project_id.write().retain(|id| id != &project_id);
                            streaming_states.lock().expect("streaming_states lock poisoned").remove(&conversation_id);
                            crate::agent::manager::AgentManager::get()
                                .register(run.into_agent());
                            return;
                        }
                        run_fut = Box::pin(run.run_until_blocked());
                    }
                    Ok(Blocked::Done(_reason)) => {
                        let ui_msg = run.finish();
                        // 替换 Streaming → Static
                        {
                            let mut all = runtimes.write();
                            if let Some(rt) = all.get_mut(&conversation_id) {
                                if let Some(pos) = rt.messages.iter().position(|m| matches!(m, UiMessage::Streaming { .. })) {
                                    rt.messages[pos] = ui_msg;
                                }
                            }
                        }

                        // 累加本次 token 用量到 conversation runtime
                        if let Some(ref usage) = run.last_usage {
                            let mut all = runtimes.write();
                            if let Some(rt) = all.get_mut(&conversation_id) {
                                rt.accumulated_usage.prompt_tokens += usage.prompt_tokens;
                                rt.accumulated_usage.completion_tokens += usage.completion_tokens;
                                rt.accumulated_usage.reasoning_tokens += usage.reasoning_tokens;
                                rt.accumulated_usage.total_tokens += usage.total_tokens;
                                rt.accumulated_usage.cache_hit_tokens += usage.cache_hit_tokens;
                                rt.accumulated_usage.cache_miss_tokens += usage.cache_miss_tokens;
                                rt.request_count += 1;
                                rt.last_loop_usage = Some(usage.clone());
                            }
                        }

                        is_streaming.set(false);
                        streaming_project_id.write().retain(|id| id != &project_id);
                        streaming_states.lock().expect("streaming_states lock poisoned").remove(&conversation_id);
                        crate::agent::manager::AgentManager::get()
                            .register(run.into_agent());
                        return;
                    }
                    Err(e) => {
                        // run 内部错误：移除 Streaming 占位 + 通知
                        {
                            let mut all = runtimes.write();
                            if let Some(rt) = all.get_mut(&conversation_id) {
                                rt.messages.retain(|m| !matches!(m, UiMessage::Streaming { .. }));
                            }
                        }
                        error_signal.write().push(ErrorInfo::new(
                            "AGENT_ERROR",
                            "Agent 执行失败",
                            format!("{}", e),
                            ErrorSeverity::Warning,
                            ErrorSource::Agent,
                        ).with_detail(format!("{:#}", e)));
                        is_streaming.set(false);
                        streaming_project_id.write().retain(|id| id != &project_id);
                        streaming_states.lock().expect("streaming_states lock poisoned").remove(&conversation_id);
                        crate::agent::manager::AgentManager::get()
                            .register(run.into_agent());
                        return;
                    }
                }
            }
            event = rx.recv(), if !rx_closed => {
                match event {
                    Some(_) => {
                        // 批量 drain：一次消费全部堆积事件，只触发一次 tick（避免重渲染风暴）
                        while rx.try_recv().is_ok() {}
                        runtimes.write().entry(conversation_id.clone()).or_default().tick += 1;
                    }
                    None => { rx_closed = true; } // 事件通道关闭（run 结束）
                }
            }
            _ = cancel_token.cancelled(), if !cancelled => {
                // 取消：标记后等 run_fut 自然返回（run 内部各 await 点响应取消）
                cancelled = true;
            }
        }
    }
}
