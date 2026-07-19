// Agent 循环 → Dioxus signals 桥接层
//
// 流式数据通过 Arc 共享，bridge 负责：
// 1. 从 AgentManager 取出 Agent 并创建 UiMessage::Streaming
// 2. 轮询 version 触发 tick_signal → UI 重渲染
// 3. Agent 完成后替换为 UiMessage::Static
// 4. 通过 AgentManager 注册回缓存

use dioxus::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use crate::agent::{ActionMode, Agent, AgentMode};
use crate::model::*;
use crate::ui::components::error::*;

pub async fn run_agent_loop(
    user_input: String,
    action_mode: ActionMode,
    agent_mode_val: AgentMode,
    mut messages: Signal<Vec<UiMessage>>,
    mut is_streaming: Signal<bool>,
    mut streaming_project_id: Signal<Option<String>>,
    cancel_token: CancellationToken,
    conversation_id: String,
    streaming_states: Arc<Mutex<HashMap<String, UiMessage>>>,
    mut tick_signal: Signal<u64>,
    mut error_signal: Signal<ErrorSignal>,
) {
    is_streaming.set(true);
    if let Err(e) = crate::agent::manager::AgentManager::get().lock()
        .unwrap().init(&conversation_id){
            error_signal.write().push(ErrorInfo::new(
                "AGENT_ERROR",
                "Agent init fail",
                format!("{}", e),
                ErrorSeverity::Warning,
                ErrorSource::Agent,
            ).with_detail(format!("{:#}", e)));
            return;
        }
    // 从 Manager 取出 Agent
    let mut agent = crate::agent::manager::AgentManager::get()
        .lock()
        .unwrap()
        .remove(&conversation_id)
        .expect("agent must be in cache before run_agent_loop");
    agent.plan_mode = action_mode;
    agent.agent_mode = agent_mode_val;

    // Agent 内部创建流式状态
    let streaming = agent.create_streaming();
    messages.write().push(streaming.clone());
    streaming_states.lock().unwrap().insert(conversation_id.clone(), streaming.clone());


    // Agent 通过 result_cell 传回结果（避免 tokio::spawn 中访问 Signal）
    let result_cell: Arc<Mutex<Option<(Agent, Result<UiMessage, anyhow::Error>)>>> = Arc::new(Mutex::new(None));
    let result_cell2 = result_cell.clone();

    tokio::spawn(async move {
        let result = agent.accept_message(user_input, cancel_token).await;
        *result_cell2.lock().unwrap() = Some((agent, result));
    });

    // 主循环：轮询 version 更新 tick_signal，检查 Agent 是否完成
    let mut last_v = 0u64;
    loop {
        tokio::time::sleep(Duration::from_millis(80)).await;

        // 从 streaming_states 中读取 version
        let v = {
            let states = streaming_states.lock().unwrap_or_else(|e| e.into_inner());
            states.get(&conversation_id).and_then(|s| {
                if let UiMessage::Streaming { version, .. } = s {
                    Some(version.load(Ordering::Relaxed))
                } else {
                    None
                }
            }).unwrap_or(0)
        };
        if v != last_v {
            last_v = v;
            tick_signal.set(v);
        }

        if let Some((ag, result)) = result_cell.lock().unwrap().take() {
            match result {
                Ok(ui_msg) => {
                    // 替换 Streaming → Static
                    {
                        let mut msgs = messages.write();
                        if let Some(pos) = msgs.iter().position(|m| matches!(m, UiMessage::Streaming { .. })) {
                            msgs[pos] = ui_msg;
                        }
                    }
                }
                Err(e) => {
                    // 移除 Streaming 占位
                    {
                        let mut msgs = messages.write();
                        msgs.retain(|m| !matches!(m, UiMessage::Streaming { .. }));
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

            is_streaming.set(false);
            streaming_project_id.set(None);
            
            streaming_states.lock().unwrap().remove(&conversation_id);
            crate::agent::manager::AgentManager::get()
                .lock()
                .unwrap()
                .register(ag);
            return;
        }
    }
}
