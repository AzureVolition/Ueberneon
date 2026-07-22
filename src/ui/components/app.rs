pub use crate::ui::components::error::{ErrorSignal, ErrorModal, ErrorBanner, ErrorToast, ErrorInfo, ErrorSeverity, ErrorSource};

use dioxus::prelude::*;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

use crate::agent::manager::AgentManager;
use crate::agent::{ActionMode, AgentHandler, AgentMode};
use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::input_bar::InputBar;
use crate::ui::components::plan_panel::PlanPanel;
use crate::ui::components::sidebar::Sidebar;
use crate::ui::components::settings_panel::SettingsPanel;
use crate::ui::state::*;
use crate::ui::state::SettingsTab;
use crate::settings;


/// Markdown 转 HTML 辅助函数
fn markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(md, pulldown_cmark::Options::ENABLE_TABLES);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

fn load_agent_configs() -> Vec<crate::db::metadata::agent_config::AgentConfigRow> {
    crate::db::with_db(|conn| crate::db::metadata::agent_config::list_all(conn).unwrap_or_default())
}


/// 从 DB 加载指定对话的消息
fn load_messages_from_db(conv_id: &str) -> Vec<ChatMessage> {
    let msgs = crate::db::with_db(|conn| {
        crate::db::metadata::message::list_as_llm_messages(conn, conv_id).unwrap_or_default()
    });
    let mut result = Vec::new();

    for m in &msgs {
        match m.role {
            llm::Role::User => {
                result.push(ChatMessage {
                    role: Role::User,
                    content: m.content.clone().unwrap_or_default(),
                    timestamp: chrono::Local::now(),
                    tool_calls: vec![], reasoning: String::new(), segments: Vec::new(),
                });
            }
            llm::Role::Assistant => {
                // 重建 ToolCallRecord（result 暂时为 None，由后续 Tool 消息回填）
                let tool_calls: Vec<ToolCallRecord> = m.tool_calls.iter().map(|tc| {
                    ToolCallRecord {
                        tool_name: tc.name.clone(),
                        args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                        result: None,
                        status: ToolCallStatus::Running,
                        approval_reason: None,
                    }
                }).collect();

                let mut segments = Vec::new();
                if let Some(ref r) = m.reasoning_content {
                    if !r.is_empty() {
                        segments.push(StreamSegment::Reasoning(r.clone()));
                    }
                }
                if let Some(ref c) = m.content {
                    if !c.is_empty() {
                        segments.push(StreamSegment::Text(c.clone()));
                    }
                }
                for _ in &m.tool_calls {
                    segments.push(StreamSegment::ToolCall);
                }

                // 如果上一条也是 Assistant（连续的 Assistant 消息），合并到同一条
                if let Some(prev) = result.last_mut().filter(|cm| cm.role == Role::Assistant) {
                    prev.segments.extend(segments);
                    prev.tool_calls.extend(tool_calls);
                    if let Some(ref c) = m.content {
                        if !c.is_empty() {
                            prev.content = c.clone();
                        }
                    }
                    if let Some(ref r) = m.reasoning_content {
                        if !r.is_empty() {
                            prev.reasoning = r.clone();
                        }
                    }
                } else {
                    result.push(ChatMessage {
                        role: Role::Assistant,
                        content: m.content.clone().unwrap_or_default(),
                        timestamp: chrono::Local::now(),
                        reasoning: m.reasoning_content.clone().unwrap_or_default(),
                        segments,
                        tool_calls,
                    });
                }
            }
            llm::Role::Tool => {
                if let (Some(_tcid), Some(content)) = (&m.tool_call_id, &m.content) {
                    let tool_name = m.tool_name.as_deref();
                    for cm in result.iter_mut().rev() {
                        if cm.role != Role::Assistant { continue; }
                        if let Some(tc) = cm.tool_calls.iter_mut().find(|tc| {
                            tc.result.is_none()
                                && tool_name.map_or(true, |n| tc.tool_name == n)
                        }) {
                            tc.result = Some(content.clone());
                            tc.status = if content.starts_with("error: denied by user:")
                                || content == "error: approval channel closed"
                            {
                                ToolCallStatus::Denied(content.clone())
                            } else if content.starts_with("error:") {
                                ToolCallStatus::Failed(content.clone())
                            } else {
                                ToolCallStatus::Success
                            };
                            break;
                        }
                    }
                }
            }
            _ => {}
        }
    }
    result
}


/// 确保对话已加载到 runtimes 中（幂等）。
/// 首次进入时从 DB 加载 + streaming_states 补全。
fn ensure_conv_loaded(
    conv_id: &str,
    mut runtimes: Signal<HashMap<String, ConversationRuntime>>,
    streaming_states: Arc<Mutex<HashMap<String, UiMessage>>>,
) {
    if runtimes.read().contains_key(conv_id) {
        return;
    }
    let mut msgs: Vec<UiMessage> = {
        load_messages_from_db(conv_id).into_iter().map(UiMessage::Static).collect()
    };
    if let Some(streaming) = streaming_states.lock().unwrap_or_else(|e| e.into_inner()).get(conv_id) {
        while msgs.last().map_or(false, |m| matches!(m, UiMessage::Static(cm) if cm.role == Role::Assistant)) {
            msgs.pop();
        }
        msgs.push(streaming.clone());
    }
    runtimes.write().insert(conv_id.to_string(), ConversationRuntime { messages: msgs, ..Default::default() });
}

#[component]
pub fn App() -> Element {
    // ── 项目状态（从 DB 加载）──
    let mut projects: Signal<Vec<Project>> = use_signal(|| {
        let rows = crate::db::with_db(|conn| {
            crate::db::metadata::project::list(conn).unwrap_or_default()
        });
        rows.into_iter()
            .map(|r| Project {
                id: r.id, name: r.name, path: r.path,
                created_at: r.created_at, conversations: Vec::new(),
                indicator_color: r.indicator_color,
                last_activity_at: r.last_activity_at,
            })
            .collect()
    });
    let mut active_project_id = use_signal(|| Option::<String>::None);
    let mut sidebar_view = use_signal(|| SidebarView::ProjectList);
    let mut active_conversation_id = use_signal(|| String::new());
    let mut runtimes = use_signal(|| HashMap::<String, ConversationRuntime>::new());
    let is_streaming = use_signal(|| false);
    let mut streaming_project_id = use_signal(Vec::<String>::new);
    let mut active_tool_calls = use_signal(Vec::<ToolCallRecord>::new);
    let action_mode = use_signal(|| ActionMode::Regular);
    let mut agent_mode = use_signal(|| AgentMode::Ask);

    // ── Agent config 选择状态 ──
    let agent_configs: Signal<Vec<crate::db::metadata::agent_config::AgentConfigRow>> = use_signal(|| {
        load_agent_configs()
    });
    let selected_agent_config_id = use_signal(|| {
        let default_id = crate::settings::get().general.default_agent_config_id;
        if !default_id.is_empty() {
            let exists = crate::db::with_db(|conn| {
                crate::db::metadata::agent_config::get(conn, &default_id)
                    .ok().flatten().is_some()
            });
            if exists { return default_id; }
        }
        String::new()
    });

    let mut pending_approval = use_signal(|| Option::<PendingApproval>::None);
    let approval_responder: Signal<Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>> =
        use_signal(|| Arc::new(Mutex::new(None)));
    let mut error_signal = use_signal(ErrorSignal::new);
    use_context_provider(|| error_signal);

    // 流式状态缓存
    let streaming_states: Arc<Mutex<HashMap<String, UiMessage>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // 审批提示文本 — PlanPanel 点击"输入修改意见"后设置，InputBar 自动填入
    let approval_hint_text: Signal<Option<String>> = use_signal(|| None);

    // 对话快照缓存（切走时暂存，切回时恢复）
    // 计划看板信号 — 从 AgentHandler 实时读取 Plan
    let plan_signal = use_memo(move || {
        let cid = active_conversation_id();
        let _ = runtimes.read().get(&cid).map(|r| r.tick).unwrap_or(0);
        if let Some(Some(agent_handler)) = runtimes.read().get(&cid).map(|r| r.agent_handler.clone()) {
            let current_plan = agent_handler.current_plan.clone();
            if let Ok(plan_guard) = current_plan.lock() {
                return plan_guard.clone();
            }
        }
        None
    });

    // ── 选择项目 ──
    let on_select_project = {
        let ss = streaming_states.clone();
        move |project_id: String| {
            active_project_id.set(Some(project_id.clone()));
            sidebar_view.set(SidebarView::ConversationList(project_id.clone()));

            // 从 DB 加载对话列表到 signal
            let convs = crate::db::with_db(|conn| {
                crate::db::metadata::conversation::list_by_project(conn, &project_id).unwrap_or_default()
            });
            let first_conv_id = convs.first().map(|c| c.id.clone());
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == project_id) {
                proj.conversations = convs.into_iter().map(|c| Conversation {
                    id: c.id, title: c.title, messages: Vec::new(), updated_at: c.updated_at,
                    message_count: c.message_count as usize,
                }).collect();
            }
            drop(projs);
            // 自动进入第一个对话
            if let Some(first_id) = first_conv_id {
                active_conversation_id.set(first_id.clone());

                // ── 从 runtime 恢复 agent_mode ──
                if let Some(rt) = runtimes.read().get(&first_id) {
                    if let Some(ref h) = rt.agent_handler {
                        agent_mode.set(*h.agent_mode.lock().expect("agent_mode lock poisoned"));
                    }
                }

                ensure_conv_loaded(&first_id, runtimes, ss.clone());
            }
        }
    };

    let on_back_to_projects = move |_| {
        sidebar_view.set(SidebarView::ProjectList);
        active_project_id.set(None);
        active_conversation_id.set(String::new());
        runtimes.write().insert(active_conversation_id(), ConversationRuntime { messages: Vec::new(), ..Default::default() });
    };

    let on_new_project = move |(name, path): (String, String)| {
        let (new_id, rows) = crate::db::with_db(|conn| {
            let new_id = crate::db::metadata::project::create(conn, &name, &path)
                .unwrap_or_else(|_| String::new());
            let rows = crate::db::metadata::project::list(conn).unwrap_or_default();
            (new_id, rows)
        });
        projects.set(
            rows.into_iter().map(|r| Project {
                id: r.id, name: r.name, path: r.path,
                created_at: r.created_at, conversations: Vec::new(),
                indicator_color: r.indicator_color, last_activity_at: r.last_activity_at,
            }).collect(),
        );
        active_project_id.set(Some(new_id.clone()));
        sidebar_view.set(SidebarView::ConversationList(new_id));
        active_conversation_id.set(String::new());
        runtimes.write().insert(active_conversation_id(), ConversationRuntime { messages: Vec::new(), ..Default::default() });
    };

    let on_new_conversation = move |_| {
        let current_proj = active_project_id.read().clone();
        let proj_id = match current_proj {
            Some(id) => id,
            None => {
                let def_id = crate::db::DEFAULT_PROJECT_ID.to_string();
                active_project_id.set(Some(def_id.clone()));
                sidebar_view.set(SidebarView::ConversationList(def_id.clone()));
                def_id
            }
        };
        let conv_id = String::new(); // 占位，真实 id 在发送时由 init_or_get 生成
        let now = chrono::Local::now();
        {
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == proj_id) {
                proj.conversations.push(Conversation {
                    id: String::new(),
                    title: String::new(),
                    messages: Vec::new(), updated_at: now,
                    message_count: 0,
                });
                proj.last_activity_at = Some(now);
            }
        }
        active_conversation_id.set(conv_id.clone());
        runtimes.write().insert(active_conversation_id(), ConversationRuntime { messages: Vec::new(), ..Default::default() });
        active_tool_calls.set(Vec::new());
    };

    let on_select_conversation = {
        let ss = streaming_states.clone();
        move |conv_id: String| {
            active_conversation_id.set(conv_id.clone());

            // ── 从 runtime 恢复 agent_mode ──
            if let Some(rt) = runtimes.read().get(&conv_id) {
                if let Some(ref h) = rt.agent_handler {
                    agent_mode.set(*h.agent_mode.lock().expect("agent_mode lock poisoned"));
                }
            }

            // ── 首次进入：从 DB 加载 ──
            ensure_conv_loaded(&conv_id, runtimes, ss.clone());
        }
    };

    let on_delete_conversation = move |conv_id: String| {
        let proj_id = active_project_id.read().clone()
            .unwrap_or_else(|| crate::db::DEFAULT_PROJECT_ID.to_string());
        if *active_conversation_id.read() == conv_id {
            active_conversation_id.set(String::new());
            runtimes.write().insert(active_conversation_id(), ConversationRuntime { messages: Vec::new(), ..Default::default() });
            active_tool_calls.set(Vec::new());
        }
        crate::db::try_with_db(|conn| {
            if let Err(e) = crate::db::metadata::message::delete_by_conversation(conn, &conv_id) { tracing::error!(target:"db", error=%e, "delete messages"); }
            if let Err(e) = crate::db::metadata::conversation::delete(conn, &conv_id) { tracing::error!(target:"db", error=%e, "delete conversation"); }
        });
        {
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == proj_id) {
                proj.conversations.retain(|c| c.id != conv_id);
            }
        }
        AgentManager::get().remove(&conv_id);
        let curr = sidebar_view.read().clone();
        sidebar_view.set(curr);
    };

    let on_delete_project = move |project_id: String| {
        if project_id == crate::db::DEFAULT_PROJECT_ID { return; }
        let is_current = active_project_id.read().as_deref() == Some(&project_id);
        if is_current {
            sidebar_view.set(SidebarView::ProjectList);
            active_project_id.set(None);
            runtimes.write().insert(active_conversation_id(), ConversationRuntime { messages: Vec::new(), ..Default::default() });
        }
        // 获取项目下的对话 ID 列表（先释放 DB 锁再操作 AgentManager）
        let conv_ids: Vec<String> = crate::db::with_db(|conn| {
            crate::db::metadata::conversation::list_by_project(conn, &project_id)
                .unwrap_or_default()
                .into_iter()
                .map(|c| c.id)
                .collect()
        });
        let mgr = AgentManager::get();
        for cid in &conv_ids { mgr.remove(cid); }
        crate::db::try_with_db(|conn| {
            if let Err(e) = crate::db::metadata::project::delete(conn, &project_id) { tracing::error!(target:"db", error=%e, "delete project"); }
        });
        let rows = crate::db::with_db(|conn| {
            crate::db::metadata::project::list(conn).unwrap_or_default()
        });
        projects.set(
            rows.into_iter().map(|r| Project {
                id: r.id, name: r.name, path: r.path,
                created_at: r.created_at, conversations: Vec::new(),
                indicator_color: r.indicator_color, last_activity_at: r.last_activity_at,
            }).collect(),
        );
    };

    let on_change_indicator_color = move |(project_id, color_key): (String, String)| {
        let rows = crate::db::with_db(|conn| {
            if let Some(mut row) = crate::db::metadata::project::get(conn, &project_id).unwrap_or(None) {
                row.indicator_color = color_key;
                if let Err(e) = crate::db::metadata::project::update(conn, &row) { tracing::error!(target:"db", error=%e, "update project"); }
            }
            crate::db::metadata::project::list(conn).unwrap_or_default()
        });
        projects.set(
            rows.into_iter().map(|r| Project {
                id: r.id, name: r.name, path: r.path,
                created_at: r.created_at, conversations: Vec::new(),
                indicator_color: r.indicator_color, last_activity_at: r.last_activity_at,
            }).collect(),
        );
    };

    rsx! {
        // ── 动态外观 CSS 变量 ──
        {
            let a = settings::get().appearance;
            let fs = match a.font_size.as_str() {
                "xs" => "0.8125rem",
                "sm" => "0.875rem",
                "md" => "1rem",
                "lg" => "1.125rem",
                "xl" => "1.25rem",
                _ => "1rem",
            };
            let cf = match a.code_font.as_str() {
                "jetbrains-mono" => "\"JetBrains Mono\",\"SF Mono\",monospace",
                "geist-mono" => "\"Geist Mono\",\"SF Mono\",monospace",
                "ibm-plex-mono" => "\"IBM Plex Mono\",\"SF Mono\",monospace",
                "commit-mono" => "\"Commit Mono\",\"SF Mono\",monospace",
                _ => "\"JetBrains Mono\",\"SF Mono\",monospace",
            };
            let compact = if a.ui_density == "compact" { "--space-sm:0.5rem;--space-md:0.75rem;--space-lg:1rem;--space-xl:1.5rem;--space-2xl:2rem;" } else { "" };
            rsx! {
                style { ":root{{--text-base:{fs};--font-mono:{cf};{compact}}}" }
            }
        }
        style { {include_str!("style.css")} }
        div {
            class: {
                let d = settings::get().appearance.ui_density;
                if d == "compact" { "app-container density-compact" } else { "app-container" }
            },
            Sidebar {
                projects,
                active_project_id,
                sidebar_view,
                active_conversation_id,
                streaming_project_id,
                on_new_project,
                on_new_conversation,
                on_select_project,
                on_select_conversation,
                on_back_to_projects,
                on_delete_project,
                on_delete_conversation,
                on_change_indicator_color,
            }
            div {
                class: "main-area",
                match sidebar_view() {
                    SidebarView::Settings(ref tab) => {
                        match tab {
                            SettingsTab::Providers | SettingsTab::General | SettingsTab::Appearance | SettingsTab::Sql | SettingsTab::Tools => {
                                rsx! {
                                    SettingsPanel {
                                        tab: tab.clone(),
                                        on_change: {
                                            let mut ac = agent_configs;
                                            move |_| { ac.set(load_agent_configs()); }
                                        },
                                    }
                                }
                            }
                            SettingsTab::AgentConfigs | SettingsTab::SubAgents => {
                                rsx! {
                                    SettingsPanel {
                                        tab: tab.clone(),
                                        on_change: {
                                            let mut ac = agent_configs;
                                            move |_| { ac.set(load_agent_configs()); }
                                        },
                                    }
                                }
                            }
                        }
                    }
                    _ => {
                        rsx! {
                            div {
                                class: "chat-area",
                                ChatPanel {
                                    runtimes,
                                    active_conv_id: active_conversation_id,
                                    is_streaming,
                                    markdown_to_html: markdown_to_html,
                                on_approve: {
                                    let resp = approval_responder;
                                    move |(allowed,): (bool,)| {
                                        if let Some(tx) = resp.read().lock().unwrap_or_else(|e| e.into_inner()).take() {
                                            let _ = tx.send(allowed);
                                        }
                                        pending_approval.set(None);
                                    }
                                },
                            }
                            PlanPanel {
                                plan: plan_signal.read().clone(),
                                on_approve: {
                                    let mut rt = runtimes;
                                    let cid = active_conversation_id();
                                    let pid = active_project_id.read().clone().unwrap_or_default();
                                    let mut am = action_mode;
                                    let is_streaming = is_streaming;
                                    let streaming_project_id = streaming_project_id;
                                    let ss = streaming_states.clone();
                                    let err_sig = error_signal;
                                    let agent_mode = agent_mode;
                                    move |()| {
                                        // 1. 切换前端 action_mode
                                        am.set(ActionMode::Regular);

                                        // 2. 执行审批
                                        let ah = rt.read().get(&cid).and_then(|r| r.agent_handler.clone());
                                        if let Some(ref h) = ah {
                                            if let Err(e) = h.approve_plan(&pid, &cid) {
                                                tracing::error!(target:"ui", error=%e, "approve_plan failed");
                                                return;
                                            }
                                            // 触发热更新，让 plan_signal 重算
                                            rt.write().entry(cid.clone()).or_default().tick += 1;

                                            // 3. 自动发送执行消息
                                            let input = "计划已通过审批，请开始执行。".to_string();
                                            let bridge_cancel = CancellationToken::new();
                                            rt.write().entry(cid.clone()).or_default().cancel_token = Some(bridge_cancel.clone());
                                            let rt2 = rt.clone();
                                            let cid2 = cid.clone();
                                            let pid2 = pid.clone();
                                            let ss2 = ss.clone();
                                            let cur_agent_mode = *agent_mode.read();
                                            spawn(async move {
                                                crate::ui::bridge::run_agent_loop(crate::ui::bridge::BridgeContext {
                                                    user_input: input,
                                                    action_mode: ActionMode::Regular,
                                                    agent_mode: cur_agent_mode,
                                                    runtimes: rt2,
                                                    is_streaming,
                                                    streaming_project_id,
                                                    project_id: pid2,
                                                    cancel_token: bridge_cancel,
                                                    conversation_id: cid2,
                                                    streaming_states: ss2,
                                                    error_signal: err_sig,
                                                }).await;
                                            });
                                        }
                                    }
                                },
                                on_reject: {
                                    let mut hint = approval_hint_text;
                                    move |()| {
                                        hint.set(Some("请对计划提出修改意见…".to_string()));
                                    }
                                },
                            }
                            }
                        InputBar {
                            is_streaming,
                            action_mode,
                            agent_mode,
                            agent_configs: agent_configs(),
                            selected_agent_config_id: selected_agent_config_id(),
                            config_disabled: !active_conversation_id.read().is_empty(),
                            approval_hint_text,
                            on_agent_config_change: {
                                let mut s_id = selected_agent_config_id;
                                move |new_id: String| {
                                    s_id.set(new_id.clone());
                                }
                            },
                            on_agent_mode_change: {
                                let mut am = agent_mode;
                                let rt_sig = runtimes;
                                let cid_sig = active_conversation_id;
                                move |new_mode: AgentMode| {
                                    am.set(new_mode);
                                    let cid = cid_sig();
                                    if let Some(rt) = rt_sig.read().get(&cid) {
                                        if let Some(ref h) = rt.agent_handler {
                                            *h.agent_mode.lock().expect("agent_mode lock poisoned") = new_mode;
                                        }
                                    }
                                }
                            },
                            on_cancel: {
                                let rt_sig = runtimes;
                                let cid_sig = active_conversation_id;
                                move |_| {
                                    let cid = cid_sig();
                                    if let Some(rt) = rt_sig.read().get(&cid) {
                                        if let Some(ref token) = rt.cancel_token {
                                            token.cancel();
                                        }
                                    }
                                }
                            },
                            on_send: {
                        let projs = projects;
                        let err_sig = error_signal;
                        move |input: String| {
                            let mut projs = projs;
                            let proj_id = active_project_id.read().clone();
                            let conv_id = active_conversation_id.read().clone();

                            let pid = match proj_id {
                                Some(ref id) => id.clone(),
                                None => {
                                    let default_id = crate::db::DEFAULT_PROJECT_ID.to_string();
                                    active_project_id.set(Some(default_id.clone()));
                                    sidebar_view.set(SidebarView::ConversationList(default_id.clone()));
                                    default_id
                                }
                            };

                            // 用户消息 → UI
                            let user_msg = ChatMessage {
                                role: Role::User,
                                content: input.clone(),
                                timestamp: chrono::Local::now(),
                                tool_calls: vec![], reasoning: String::new(), segments: Vec::new(),
                            };

                            // 对话 ID
                            let cid = if conv_id.is_empty() {
                                // 新对话：由 init_or_get 生成 id 并创建 DB 行
                                let mgr = AgentManager::get();
                                let current_ac_id = selected_agent_config_id.read().clone();
                                let ac_id_for_conv: Option<&str> = if current_ac_id.is_empty() { None } else { Some(current_ac_id.as_str()) };
                                let (new_cid, handler) = mgr.init_or_get(
                                    None,
                                    Some(pid.clone()),
                                    ac_id_for_conv,
                                    None,
                                ).unwrap_or_else(|_| (String::new(), AgentHandler::default()));
                                runtimes.write().entry(new_cid.clone()).or_default().agent_handler = Some(handler);
                                let now = chrono::Local::now();
                                {
                                    let mut p = projs.write();
                                    if let Some(proj) = p.iter_mut().find(|p| p.id == pid) {
                                        proj.conversations.push(Conversation {
                                            id: new_cid.clone(), title: crate::model::title_from_messages(&[user_msg.clone()]),
                                            messages: Vec::new(), updated_at: now,
                                            message_count: 0,
                                        });
                                        // 移除旧的占位条目（只删当前对话对应的占位）
                                        if let Some(pos) = proj.conversations.iter().position(|c| c.id.is_empty()) {
                                            proj.conversations.remove(pos);
                                        }
                                    }
                                }
                                active_conversation_id.set(new_cid.clone());
                                new_cid
                            } else {
                                // 已有对话：确保 Agent 在缓存中，获取 handler
                                let mgr = AgentManager::get();
                                if let Ok(handler) = mgr.init(&conv_id) {
                                    runtimes.write().entry(conv_id.clone()).or_default().agent_handler = Some(handler);
                                }
                                conv_id
                            };
                            runtimes.write().entry(cid.clone()).or_default().messages.push(UiMessage::Static(user_msg.clone()));

                            // DB 更新对话标题 + 项目活跃时间
                            crate::db::try_with_db(|conn| {
                                conn.execute(
                                    "UPDATE conversations SET title = ?1 WHERE id = ?2 and (title = '' or title is null)",
                                    rusqlite::params![crate::model::title_from_messages(&[user_msg.clone()]), cid],
                                ).unwrap_or_else(|e| { tracing::error!(target:"db", error=%e, "update conversation title"); 0 });
                                if let Err(e) = crate::db::metadata::project::touch(conn, &pid) { tracing::error!(target:"db", error=%e, "touch project"); }
                            });
                            // 刷新 signal（标题、轮数、last_activity_at 同步）
                            let convs = crate::db::with_db(|conn| {
                                crate::db::metadata::conversation::list_by_project(conn, &pid).unwrap_or_default()
                            });
                            let mut p = projs.write();
                                if let Some(proj) = p.iter_mut().find(|pr| pr.id == pid) {
                                    proj.conversations = convs.into_iter().map(|c| Conversation {
                                        id: c.id, title: c.title, messages: Vec::new(),
                                        updated_at: c.updated_at, message_count: c.message_count as usize,
                                    }).collect();
                                    proj.last_activity_at = Some(chrono::Local::now());
                                }
                                drop(p);
                            streaming_project_id.write().push(pid.clone());

                            
                            let cur_action_mode = action_mode();
                            let cur_agent_mode = agent_mode();
                            let bridge_cancel = CancellationToken::new();
                            runtimes.write().entry(cid.clone()).or_default().cancel_token = Some(bridge_cancel.clone());

                            let ss = streaming_states.clone();
                            spawn(async move {
                                crate::ui::bridge::run_agent_loop(crate::ui::bridge::BridgeContext {
                                    user_input: input,
                                    action_mode: cur_action_mode,
                                    agent_mode: cur_agent_mode,
                                    runtimes,
                                    is_streaming,
                                    streaming_project_id,
                                    project_id: pid.clone(),
                                    cancel_token: bridge_cancel,
                                    conversation_id: cid,
                                    streaming_states: ss,
                                    error_signal: err_sig,
                                }).await;
                            });
                        }
                    },
                }  // ← InputBar
            }  // ← rsx!
            }  // ← _ => arm
            }  // ← match
            }  // ← main-area

            if let Some(ref err) = error_signal.read().modal.clone() {
                ErrorModal { error: err.clone(), on_dismiss: move |_| error_signal.write().dismiss_modal() }
            }
            if let Some(ref err) = error_signal.read().banner.clone() {
                ErrorBanner { error: err.clone(), on_dismiss: move |_| error_signal.write().dismiss_banner() }
            }
            if !error_signal.read().toasts.is_empty() {
                div {
                    class: "error-toast-stack",
                    for (i, toast) in error_signal.read().toasts.iter().enumerate() {
                        ErrorToast {
                            key: "toast-{i}",
                            error: toast.clone(),
                            on_dismiss: move |_| error_signal.write().dismiss_toast(i),
                        }
                    }
                }
            }
        }
    }
}
