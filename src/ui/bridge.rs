// Agent 循环 → Dioxus signals 桥接层
//
// 流式数据通过 Arc 共享，bridge 负责：
// 1. 从 AgentManager 取出 Agent 并创建 UiMessage::Streaming
// 2. 轮询 version 触发 tick_signal → UI 重渲染
// 3. Agent 完成后替换为 UiMessage::Static
// 4. 通过 AgentManager 注册回缓存

use dioxus::prelude::*;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::{ActionMode, AgentMode, AgentRun};
use crate::model::*;
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
    let (mut run, mut rx) = AgentRun::new(agent);
    run.agent.handler.set_action_mode(action_mode);
    *run.agent.handler.agent_mode.lock().expect("agent_mode lock poisoned") = agent_mode_val;

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


    // 完成信号：watch 通道（spawn 侧完成后置 true，主循环据此收尾）
    let (done_tx, mut done_rx) = tokio::sync::watch::channel(false);

    // Run 通过 result_cell 传回结果（避免 tokio::spawn 中访问 Signal）
    let result_cell: Arc<Mutex<Option<(AgentRun, Result<UiMessage, anyhow::Error>)>>> = Arc::new(Mutex::new(None));
    let result_cell2 = result_cell.clone();
    let done_tx2 = done_tx.clone();

    let cancel_for_spawn = cancel_token.clone();
    tokio::spawn(async move {
        let result = run.accept_message(user_input, cancel_for_spawn).await;
        *result_cell2.lock().expect("result_cell2 lock poisoned") = Some((run, result));
        let _ = done_tx2.send(true);
    });

    // 主循环：事件驱动 —— 任何事件触发一次重渲染；done 信号收尾；cancel 中断
    let mut cancelled = false;
    let mut rx_closed = false;
    loop {
        tokio::select! {
            event = rx.recv(), if !rx_closed => {
                match event {
                    Some(_) => {
                        // 有新状态（文本/工具/审批/错误），触发 UI 重渲染
                        runtimes.write().entry(conversation_id.clone()).or_default().tick += 1;
                    }
                    None => { rx_closed = true; } // 事件通道关闭（run 结束），等 done
                }
            }
            _ = done_rx.changed() => {
                if let Some((run, result)) = result_cell.lock().expect("result_cell lock poisoned").take() {
                    match result {
                        Ok(ui_msg) => {
                            // 替换 Streaming → Static
                            {
                                let mut all = runtimes.write();
                                if let Some(rt) = all.get_mut(&conversation_id) {
                                    if let Some(pos) = rt.messages.iter().position(|m| matches!(m, UiMessage::Streaming { .. })) {
                                        rt.messages[pos] = ui_msg;
                                    }
                                }
                            }
                        }
                        Err(e) => {
                            // 移除 Streaming 占位
                            {
                                let mut all = runtimes.write();
                                if let Some(rt) = all.get_mut(&conversation_id) {
                                    rt.messages.retain(|m| !matches!(m, UiMessage::Streaming { .. }));
                                }
                            }
                            // 通过 error_signal 通知前端
                            error_signal.write().push(ErrorInfo::new(
                                "AGENT_ERROR",
                                "Agent 执行失败",
                                format!("{}", e),
                                ErrorSeverity::Warning,
                                ErrorSource::Agent,
                            ).with_detail(format!("{:#}", e)));
                        }
                    }

                    // 累加本次 token 用量到 conversation runtime，
                    // 同时保存单次 loop 数据（暂不展示，预留）
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
            }
            _ = cancel_token.cancelled(), if !cancelled => {
                // 取消：标记后继续等 done（accept_message 内部响应取消并返回）
                cancelled = true;
            }
        }
    }
}
