pub use crate::ui::components::error::{
    ErrorBanner, ErrorInfo, ErrorModal, ErrorSeverity, ErrorSignal, ErrorSource, ErrorToast,
};

use dioxus::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::manager::AgentManager;
use crate::agent::{ActionMode, AgentHandler, AgentMode};
use crate::settings;
use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::dashboard_panel::DashboardPanel;
use crate::ui::components::input_bar::InputBar;
use crate::ui::components::plan_panel::PlanPanel;
use crate::ui::components::settings_panel::SettingsPanel;
use crate::ui::components::sidebar::Sidebar;
use crate::ui::state::SettingsTab;
use crate::ui::state::*;

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
fn load_messages_from_db(conv_id: &str, md_to_html: fn(&str) -> String) -> Vec<ChatMessage> {
    let msgs = crate::db::with_db(|conn| {
        crate::db::metadata::message::list_as_llm_messages(conn, conv_id).unwrap_or_default()
    });
    let mut result = Vec::new();

    for m in &msgs {
        match m.role {
            llm::Role::User => {
                let content = m.content.clone().unwrap_or_default();
                let html = md_to_html(&content);
                result.push(ChatMessage {
                    role: Role::User,
                    content,
                    timestamp: chrono::Local::now(),
                    reasoning: String::new(),
                    segments: Vec::new(),
                    content_html: html,
                });
            }
            llm::Role::Assistant => {
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
                // 工具调用记录内嵌进 segments（result 暂时为 None，由后续 Tool 消息回填）
                for tc in &m.tool_calls {
                    segments.push(StreamSegment::ToolCall(ToolCallRecord {
                        id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                        result: None,
                        status: ToolCallStatus::Running,
                        approval_reason: None,
                    }));
                }

                // 如果上一条也是 Assistant（连续的 Assistant 消息），合并到同一条
                if let Some(prev) = result.last_mut().filter(|cm| cm.role == Role::Assistant) {
                    prev.segments.extend(segments);
                    if let Some(ref c) = m.content {
                        if !c.is_empty() {
                            prev.content = c.clone();
                            prev.content_html = md_to_html(c);
                        }
                    }
                    if let Some(ref r) = m.reasoning_content {
                        if !r.is_empty() {
                            prev.reasoning = r.clone();
                        }
                    }
                } else {
                    let content = m.content.clone().unwrap_or_default();
                    let html = md_to_html(&content);
                    result.push(ChatMessage {
                        role: Role::Assistant,
                        content,
                        timestamp: chrono::Local::now(),
                        reasoning: m.reasoning_content.clone().unwrap_or_default(),
                        segments,
                        content_html: html,
                    });
                }
            }
            llm::Role::Tool => {
                if let (Some(_tcid), Some(content)) = (&m.tool_call_id, &m.content) {
                    let tool_name = m.tool_name.as_deref();
                    for cm in result.iter_mut().rev() {
                        if cm.role != Role::Assistant {
                            continue;
                        }
                        if let Some(tc) = cm.segments.iter_mut().find_map(|s| match s {
                            StreamSegment::ToolCall(rec) if rec.result.is_none() && tool_name.map_or(true, |n| rec.tool_name == n) => Some(rec),
                            _ => None,
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
    md_to_html: fn(&str) -> String,
) {
    if runtimes.read().contains_key(conv_id) {
        return;
    }
    // 从 DB 读取该对话的 agent_config_id
    let agent_config_id: Option<String> = crate::db::with_db(|conn| {
        crate::db::metadata::conversation::get(conn, conv_id)
            .ok()
            .flatten()
            .and_then(|c| c.agent_config_id)
    });
    // 从 settings 读取默认 agent_mode
    let agent_mode: AgentMode = crate::settings::get()
        .general
        .default_agent_mode
        .parse()
        .unwrap_or_default();
    let mut msgs: Vec<UiMessage> = {
        load_messages_from_db(conv_id, md_to_html)
            .into_iter()
            .map(UiMessage::Static)
            .collect()
    };
    if let Some(streaming) = streaming_states
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(conv_id)
    {
        while msgs.last().map_or(
            false,
            |m| matches!(m, UiMessage::Static(cm) if cm.role == Role::Assistant),
        ) {
            msgs.pop();
        }
        msgs.push(streaming.clone());
    }
    // 从 DB 恢复该对话的累计 token 用量
    let (db_usage, db_requests, db_context_window) = crate::db::with_db(|conn| {
        let usage = crate::db::metadata::conversation::get_usage(conn, conv_id)
            .unwrap_or_default();
        let reqs = crate::db::metadata::conversation::get_request_count(conn, conv_id)
            .unwrap_or(0);
        let cw = agent_config_id.as_ref()
            .and_then(|acid| crate::db::metadata::agent_config::get(conn, acid).ok().flatten())
            .and_then(|ac| ac.context_window)
            .unwrap_or(crate::model::DEFAULT_CONTEXT_WINDOW);
        (usage, reqs, cw)
    });

    runtimes.write().insert(
        conv_id.to_string(),
        ConversationRuntime {
            messages: msgs,
            agent_config_id,
            agent_mode,
            accumulated_usage: db_usage,
            request_count: db_requests,
            context_window: db_context_window,
            ..Default::default()
        },
    );
}

#[component]
pub fn App() -> Element {
    // ── 项目状态（从 DB 加载）──
    let mut projects: Signal<Vec<Project>> = use_signal(|| {
        let rows =
            crate::db::with_db(|conn| crate::db::metadata::project::list(conn).unwrap_or_default());
        rows.into_iter()
            .map(|r| Project {
                id: r.id,
                name: r.name,
                path: r.path,
                created_at: r.created_at,
                conversations: Vec::new(),
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
    let mut agent_mode = use_signal(|| {
        crate::settings::get()
            .general
            .default_agent_mode
            .parse::<AgentMode>()
            .unwrap_or_default()
    });

    // ── Agent config 选择状态 ──
    let agent_configs: Signal<Vec<crate::db::metadata::agent_config::AgentConfigRow>> =
        use_signal(|| load_agent_configs().into_iter().filter(|c| c.agent_type != "SubAgent").collect());
    let mut selected_agent_config_id = use_signal(|| {
        let default_id = crate::settings::get().general.default_agent_config_id;
        if !default_id.is_empty() {
            let exists = crate::db::with_db(|conn| {
                crate::db::metadata::agent_config::get(conn, &default_id)
                    .ok()
                    .flatten()
                    .is_some()
            });
            if exists {
                return default_id;
            }
        }
        String::new()
    });

    // 审批注入通道（按 conversation_id 键控，避免多对话并发串台）：
    // bridge 在工具执行阶段写入 mpsc Sender，审批卡按钮经它发送 (tool_call_id, approved)
    let approval_tx: Signal<std::collections::HashMap<String, tokio::sync::mpsc::Sender<(String, bool)>>> =
        use_signal(std::collections::HashMap::new);
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
        if let Some(Some(agent_handler)) =
            runtimes.read().get(&cid).map(|r| r.agent_handler.clone())
        {
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
                crate::db::metadata::conversation::list_by_project(conn, &project_id)
                    .unwrap_or_else(|e| {
                        tracing::error!(target: "db", error = %e, project_id = %project_id, "list_by_project failed in on_select_project");
                        Vec::new()
                    })
            });
            let first_conv_id = convs.first().map(|c| c.id.clone());
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == project_id) {
                proj.conversations = convs
                    .into_iter()
                    .map(|c| Conversation {
                        id: c.id,
                        title: c.title,
                        messages: Vec::new(),
                        updated_at: c.updated_at,
                        message_count: c.message_count as usize,
                    })
                    .collect();
            }
            drop(projs);
            // 自动进入第一个对话
            if let Some(first_id) = first_conv_id {
                active_conversation_id.set(first_id.clone());

                ensure_conv_loaded(&first_id, runtimes, ss.clone(), markdown_to_html);

                // 同步 agent_mode 到 signal
                if let Some(rt) = runtimes.read().get(&first_id) {
                    agent_mode.set(rt.agent_mode);
                }
            }
        }
    };

    let on_back_to_projects = move |_| {
        sidebar_view.set(SidebarView::ProjectList);
        active_project_id.set(None);
        active_conversation_id.set(String::new());
        runtimes.write().insert(
            active_conversation_id(),
            ConversationRuntime {
                messages: Vec::new(),
                ..Default::default()
            },
        );
    };

    let on_new_project = move |(name, path): (String, String)| {
        let (new_id, rows) = crate::db::with_db(|conn| {
            let new_id = crate::db::metadata::project::create(conn, &name, &path)
                .unwrap_or_else(|_| String::new());
            let rows = crate::db::metadata::project::list(conn).unwrap_or_default();
            (new_id, rows)
        });
        projects.set(
            rows.into_iter()
                .map(|r| Project {
                    id: r.id,
                    name: r.name,
                    path: r.path,
                    created_at: r.created_at,
                    conversations: Vec::new(),
                    indicator_color: r.indicator_color,
                    last_activity_at: r.last_activity_at,
                })
                .collect(),
        );
        active_project_id.set(Some(new_id.clone()));
        sidebar_view.set(SidebarView::ConversationList(new_id));
        active_conversation_id.set(String::new());
        runtimes.write().insert(
            active_conversation_id(),
            ConversationRuntime {
                messages: Vec::new(),
                ..Default::default()
            },
        );
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
                    messages: Vec::new(),
                    updated_at: now,
                    message_count: 0,
                });
                proj.last_activity_at = Some(now);
            }
        }
        active_conversation_id.set(conv_id.clone());
        agent_mode.set(
            crate::settings::get()
                .general.default_agent_mode
                .parse::<AgentMode>()
                .unwrap_or_default(),
        );
        // 同步默认 agent config id（跟 agent_mode 一样从 settings 重新读取，
        // 避免用户在 settings 面板修改后不重启就不生效）
        {
            let default_ac_id = crate::settings::get().general.default_agent_config_id.clone();
            if !default_ac_id.is_empty() {
                let exists = crate::db::with_db(|conn| {
                    crate::db::metadata::agent_config::get(conn, &default_ac_id)
                        .ok()
                        .flatten()
                        .is_some()
                });
                selected_agent_config_id.set(if exists { default_ac_id } else { String::new() });
            } else {
                selected_agent_config_id.set(String::new());
            }
        }
        runtimes.write().insert(
            active_conversation_id(),
            ConversationRuntime {
                messages: Vec::new(),
                ..Default::default()
            },
        );
        active_tool_calls.set(Vec::new());
    };

    let on_select_conversation = {
        let ss = streaming_states.clone();
        let mut am = agent_mode;
        move |conv_id: String| {
            active_conversation_id.set(conv_id.clone());

            // ── 首次进入：从 DB 加载消息（内含 agent_config_id 写入 runtime）──
            ensure_conv_loaded(&conv_id, runtimes, ss.clone(), markdown_to_html);

            // ── 同步 agent_mode 到 signal（覆盖新对话默认值）──
            if let Some(rt) = runtimes.read().get(&conv_id) {
                am.set(rt.agent_mode);
            }
        }
    };

    let on_delete_conversation = move |conv_id: String| {
        let proj_id = active_project_id
            .read()
            .clone()
            .unwrap_or_else(|| crate::db::DEFAULT_PROJECT_ID.to_string());
        if *active_conversation_id.read() == conv_id {
            active_conversation_id.set(String::new());
            runtimes.write().insert(
                active_conversation_id(),
                ConversationRuntime {
                    messages: Vec::new(),
                    ..Default::default()
                },
            );
            active_tool_calls.set(Vec::new());
        }
        crate::db::try_with_db(|conn| {
            if let Err(e) = crate::db::metadata::message::delete_by_conversation(conn, &conv_id) {
                tracing::error!(target:"db", error=%e, "delete messages");
            }
            if let Err(e) = crate::db::metadata::conversation::delete(conn, &conv_id) {
                tracing::error!(target:"db", error=%e, "delete conversation");
            }
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
        if project_id == crate::db::DEFAULT_PROJECT_ID {
            return;
        }
        let is_current = active_project_id.read().as_deref() == Some(&project_id);
        if is_current {
            sidebar_view.set(SidebarView::ProjectList);
            active_project_id.set(None);
            runtimes.write().insert(
                active_conversation_id(),
                ConversationRuntime {
                    messages: Vec::new(),
                    ..Default::default()
                },
            );
        }
        // 获取项目下的对话 ID 列表（先释放 DB 锁再操作 AgentManager）
        let conv_ids: Vec<String> = crate::db::with_db(|conn| {
            crate::db::metadata::conversation::list_by_project(conn, &project_id)
                .unwrap_or_else(|e| {
                    tracing::error!(target: "db", error = %e, project_id = %project_id, "list_by_project failed in delete_project");
                    Vec::new()
                })
                .into_iter()
                .map(|c| c.id)
                .collect()
        });
        let mgr = AgentManager::get();
        for cid in &conv_ids {
            mgr.remove(cid);
        }
        crate::db::try_with_db(|conn| {
            if let Err(e) = crate::db::metadata::project::delete(conn, &project_id) {
                tracing::error!(target:"db", error=%e, "delete project");
            }
        });
        let rows =
            crate::db::with_db(|conn| crate::db::metadata::project::list(conn).unwrap_or_default());
        projects.set(
            rows.into_iter()
                .map(|r| Project {
                    id: r.id,
                    name: r.name,
                    path: r.path,
                    created_at: r.created_at,
                    conversations: Vec::new(),
                    indicator_color: r.indicator_color,
                    last_activity_at: r.last_activity_at,
                })
                .collect(),
        );
    };

    let on_change_indicator_color = move |(project_id, color_key): (String, String)| {
        let rows = crate::db::with_db(|conn| {
            if let Some(mut row) =
                crate::db::metadata::project::get(conn, &project_id).unwrap_or(None)
            {
                row.indicator_color = color_key;
                if let Err(e) = crate::db::metadata::project::update(conn, &row) {
                    tracing::error!(target:"db", error=%e, "update project");
                }
            }
            crate::db::metadata::project::list(conn).unwrap_or_default()
        });
        projects.set(
            rows.into_iter()
                .map(|r| Project {
                    id: r.id,
                    name: r.name,
                    path: r.path,
                    created_at: r.created_at,
                    conversations: Vec::new(),
                    indicator_color: r.indicator_color,
                    last_activity_at: r.last_activity_at,
                })
                .collect(),
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
                                            let mut sel_id = selected_agent_config_id;
                                            move |_| {
                                                ac.set(load_agent_configs().into_iter().filter(|c| c.agent_type != "SubAgent").collect());
                                                // 同步默认 agent config id，确保 settings 修改后 UI 立即反映
                                                let default_id = crate::settings::get().general.default_agent_config_id.clone();
                                                if !default_id.is_empty() {
                                                    let exists = crate::db::with_db(|conn| {
                                                        crate::db::metadata::agent_config::get(conn, &default_id)
                                                            .ok()
                                                            .flatten()
                                                            .is_some()
                                                    });
                                                    sel_id.set(if exists { default_id } else { String::new() });
                                                }
                                            }
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
                                            let mut sel_id = selected_agent_config_id;
                                            move |_| {
                                                ac.set(load_agent_configs().into_iter().filter(|c| c.agent_type != "SubAgent").collect());
                                                let default_id = crate::settings::get().general.default_agent_config_id.clone();
                                                if !default_id.is_empty() {
                                                    let exists = crate::db::with_db(|conn| {
                                                        crate::db::metadata::agent_config::get(conn, &default_id)
                                                            .ok()
                                                            .flatten()
                                                            .is_some()
                                                    });
                                                    sel_id.set(if exists { default_id } else { String::new() });
                                                }
                                            }
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
                                    let atx = approval_tx;
                                    let cid = active_conversation_id;
                                    move |(tool_call_id, allowed): (String, bool)| {
                                        // 按当前对话取审批通道发送 (tool_call_id, approved)。
                                        // onclick 是同步上下文，mpsc::Sender::send 是 async，
                                        // 必须用 try_send（审批消息量小，32 容量不会满）
                                        let conv_id = cid();
                                        if let Some(tx) = atx.read().get(&conv_id).cloned() {
                                            let _ = tx.try_send((tool_call_id, allowed));
                                        }
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

                                            // 3. 将用户消息写入对话列表（前端可见）
                                            let user_msg = crate::model::UiMessage::Static(crate::model::ChatMessage {
                                                role: crate::model::Role::User,
                                                content: "计划已通过审批，请开始执行。".to_string(),
                                                timestamp: chrono::Local::now(),
                                                reasoning: String::new(),
                                                segments: Vec::new(),
                                                content_html: String::new(),
                                            });
                                            rt.write().entry(cid.clone()).or_default().messages.push(user_msg);
                                            rt.write().entry(cid.clone()).or_default().tick += 1;

                                            // 4. 自动发送执行消息
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
                                                    approval_tx,
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
                            selected_agent_config_id: {
                                let cid = active_conversation_id();
                                if cid.is_empty() {
                                    selected_agent_config_id()
                                } else {
                                    runtimes.read().get(&cid)
                                        .and_then(|rt| rt.agent_config_id.clone())
                                        .unwrap_or_default()
                                }
                            },
                            config_disabled: !active_conversation_id.read().is_empty(),
                            approval_hint_text,
                            on_agent_config_change: {
                                let mut sel_id = selected_agent_config_id;
                                let mut rt_sig = runtimes;
                                let cid_sig = active_conversation_id;
                                move |new_id: String| {
                                    sel_id.set(new_id.clone());
                                    let cid = cid_sig();
                                    let mut rts = rt_sig.write();
                                    if let Some(rt) = rts.get_mut(&cid) {
                                        rt.agent_config_id = Some(new_id);
                                    }
                                }
                            },
                            on_agent_mode_change: {
                                let mut am = agent_mode;
                                let mut rt_sig = runtimes;
                                let cid_sig = active_conversation_id;
                                move |new_mode: AgentMode| {
                                    am.set(new_mode);
                                    let cid = cid_sig();
                                    let mut rts = rt_sig.write();
                                    if let Some(rt) = rts.get_mut(&cid) {
                                        rt.agent_mode = new_mode;
                                        // 同步到 agent_handler（持久化、runtime 重建时使用）
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
                                reasoning: String::new(), segments: Vec::new(),
                                content_html: markdown_to_html(&input),
                            };

                            // 对话 ID
                            let cid = if conv_id.is_empty() {
                                // 新对话：由 init_or_get 生成 id 并创建 DB 行
                                let mgr = AgentManager::get();
                                let signal_ac_id = selected_agent_config_id.read().clone();
                                let current_ac_id = runtimes.read().get(&conv_id)
                                    .and_then(|rt| rt.agent_config_id.clone())
                                    .filter(|s| !s.is_empty())
                                    .or_else(|| {
                                        (!signal_ac_id.is_empty()).then_some(signal_ac_id)
                                    })
                                    .unwrap_or_default();
                                let ac_id_for_conv: Option<&str> = if current_ac_id.is_empty() { None } else { Some(current_ac_id.as_str()) };
                                let (new_cid, handler) = mgr.init_or_get(
                                    None,
                                    Some(pid.clone()),
                                    ac_id_for_conv,
                                    None,
                                ).unwrap_or_else(|_| (String::new(), AgentHandler::default()));
                                runtimes.write().entry(new_cid.clone()).or_default().agent_handler = Some(handler);
                                if !current_ac_id.is_empty() {
                                    runtimes.write().entry(new_cid.clone()).or_default().agent_config_id = Some(current_ac_id);
                                }
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
                                // （缓存命中 → Ok(None)，沿用 runtime 里已有的 handler，
                                //   其 current_plan 等状态跨 run 保留，不被覆盖）
                                let mgr = AgentManager::get();
                                if let Ok(Some(handler)) = mgr.init(&conv_id) {
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
                                crate::db::metadata::conversation::list_by_project(conn, &pid)
                                    .unwrap_or_else(|e| {
                                        tracing::error!(target: "db", error = %e, project_id = %pid, "list_by_project failed in refresh");
                                        Vec::new()
                                    })
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
                                    approval_tx,
                                }).await;
                            });
                        }
                    },
                }  // ← InputBar
                DashboardPanel {
                    usage: {
                        let cid = active_conversation_id();
                        runtimes.read().get(&cid)
                            .map(|r| r.accumulated_usage.clone())
                            .unwrap_or_default()
                    },
                    request_count: {
                        let cid = active_conversation_id();
                        runtimes.read().get(&cid)
                            .map(|r| r.request_count)
                            .unwrap_or(0)
                    },
                    context_window: {
                        let cid = active_conversation_id();
                        runtimes.read().get(&cid)
                            .map(|r| r.context_window)
                            .unwrap_or(1000000)
                    },
                    last_prompt_tokens: {
                        let cid = active_conversation_id();
                        runtimes.read().get(&cid)
                            .and_then(|r| r.last_loop_usage.as_ref())
                            .map(|u| u.prompt_tokens)
                    },
                }
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
