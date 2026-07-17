use dioxus::prelude::*;
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use tokio_util::sync::CancellationToken;

use crate::agent::manager::AgentManager;
use crate::agent::{ActionMode, AgentMode};
use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::input_bar::InputBar;
use crate::ui::components::sidebar::Sidebar;
use crate::ui::components::settings_panel::SettingsPanel;
use crate::ui::components::error::{ErrorSignal, ErrorModal, ErrorBanner, ErrorToast, ErrorInfo, ErrorSeverity, ErrorSource};
use crate::ui::state::*;
use crate::agent::main_agent_prompt;

/// Markdown 转 HTML 辅助函数
fn markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(md, pulldown_cmark::Options::ENABLE_TABLES);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

/// 从 DB 加载指定对话的消息
fn load_messages_from_db(conv_id: &str) -> Vec<ChatMessage> {
    let conn = crate::db::get_db().lock().unwrap();
    let msgs = crate::db::metadata::message::list_as_llm_messages(&conn, conv_id).unwrap_or_default();
    drop(conn);
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

                // 构建 segments：reasoning → text → tool call markers
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

                result.push(ChatMessage {
                    role: Role::Assistant,
                    content: m.content.clone().unwrap_or_default(),
                    timestamp: chrono::Local::now(),
                    reasoning: m.reasoning_content.clone().unwrap_or_default(),
                    segments,
                    tool_calls,
                });
            }
            llm::Role::Tool => {
                // 从后往前找匹配的 Assistant，回填 tool 结果
                if let (Some(_tcid), Some(content)) = (&m.tool_call_id, &m.content) {
                    let tool_name = m.name.as_deref();
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

/// 从 DB 取第一个 provider 实例初始化全局 Agent 配置
fn init_agent_from_first_instance() {
    let conn = crate::db::get_db().lock().unwrap();
    let instances = crate::db::metadata::provider_instance::list_all(&conn).unwrap_or_default();
    if let Some(inst) = instances.first() {
        if let Ok(Some(prov)) = crate::db::metadata::provider::get(&conn, &inst.provider_id) {
            let models = crate::db::metadata::provider::list_models(&conn, &prov.id).unwrap_or_default();
            let model = models.first().cloned().unwrap_or_else(|| {
                crate::db::provider_presets::all_presets().iter()
                    .find(|p| p.id == prov.id)
                    .and_then(|p| p.models.first().copied())
                    .unwrap_or("")
                    .to_string()
            });
            let api_key = if !inst.api_key.is_empty() {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(inst.api_key.as_bytes())
                    .ok().and_then(|v| String::from_utf8(v).ok())
                    .unwrap_or_default()
            } else {
                String::new()
            };
            crate::agent::manager::init_global_config(crate::agent::manager::AgentConfig {
                model,
                base_url: prov.base_url,
                api_key,
            });
        }
    }
    drop(conn);
}

#[component]
pub fn App() -> Element {
    // ── 初始化全局 Agent 配置（取第一个可用实例）──
    init_agent_from_first_instance();

    // ── 项目状态（从 DB 加载）──
    let mut projects: Signal<Vec<Project>> = use_signal(|| {
        let conn = crate::db::get_db().lock().unwrap();
        let rows = crate::db::metadata::project::list(&conn).unwrap_or_default();
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
    let mut messages = use_signal(Vec::<UiMessage>::new);
    let mut streaming_segments = use_signal(Vec::<StreamSegment>::new);
    let mut is_streaming = use_signal(|| false);
    let mut tick = use_signal(|| 0u64);
    let mut streaming_project_id = use_signal(|| Option::<String>::None);
    let mut active_tool_calls = use_signal(Vec::<ToolCallRecord>::new);
    let mut action_mode = use_signal(|| ActionMode::Regular);
    let mut agent_mode = use_signal(|| AgentMode::Ask);
    let mut pending_approval = use_signal(|| Option::<PendingApproval>::None);
    let approval_responder: Signal<Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>> =
        use_signal(|| Arc::new(Mutex::new(None)));
    let mut cancel_token: Signal<Option<CancellationToken>> = use_signal(|| None);
    let mut error_signal = use_signal(ErrorSignal::new);

    // 流式状态缓存
    let streaming_states: Arc<Mutex<HashMap<String, UiMessage>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // ── 选择项目 ──
    let mut on_select_project = {
        let ss = streaming_states.clone();
        move |project_id: String| {
            active_project_id.set(Some(project_id.clone()));
            sidebar_view.set(SidebarView::ConversationList(project_id.clone()));

            // 从 DB 加载对话列表到 signal
            if let Ok(conn) = crate::db::get_db().lock() {
                let convs = crate::db::metadata::conversation::list_by_project(&conn, &project_id).unwrap_or_default();
                let mut projs = projects.write();
                if let Some(proj) = projs.iter_mut().find(|p| p.id == project_id) {
                    proj.conversations = convs.into_iter().map(|c| Conversation {
                        id: c.id, title: c.title, messages: Vec::new(), updated_at: c.updated_at,
                        message_count: c.message_count as usize,
                    }).collect();
                    crate::db::metadata::project::touch(&conn, &project_id).ok();
                }
                drop(projs);
            }

            let projs = projects.read();
            if let Some(proj) = projs.iter().find(|p| p.id == project_id) {
                if let Some(first) = proj.conversations.first() {
                    let cid = first.id.clone();
                    drop(projs);
                    active_conversation_id.set(cid.clone());
                    let mut msgs: Vec<UiMessage> = {
                        load_messages_from_db(&cid).into_iter().map(UiMessage::Static).collect()
                    };
                    if let Some(streaming) = ss.lock().unwrap_or_else(|e| e.into_inner()).get(&cid) {
                        msgs.push(streaming.clone());
                    }
                    messages.set(msgs);
                    return;
                }
            }
            active_conversation_id.set(String::new());
            messages.set(Vec::new());
            streaming_segments.set(Vec::new());
        }
    };

    let on_back_to_projects = move |_| {
        sidebar_view.set(SidebarView::ProjectList);
        active_project_id.set(None);
        active_conversation_id.set(String::new());
        messages.set(Vec::new());
        streaming_segments.set(Vec::new());
    };

    let on_open_settings = move |_| {
        sidebar_view.set(SidebarView::Settings);
    };

    let on_new_project = move |(name, path): (String, String)| {
        let conn = crate::db::get_db().lock().unwrap();
        let new_id = crate::db::metadata::project::create(&conn, &name, &path)
            .unwrap_or_else(|_| String::new());
        let rows = crate::db::metadata::project::list(&conn).unwrap_or_default();
        drop(conn);
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
        messages.set(Vec::new());
        streaming_segments.set(Vec::new());
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
        messages.set(Vec::new());
        streaming_segments.set(Vec::new());
        active_tool_calls.set(Vec::new());
    };

    let on_select_conversation = {
        let ss = streaming_states.clone();
        move |conv_id: String| {
            let proj_id = active_project_id.read().clone()
                .unwrap_or_else(|| crate::db::DEFAULT_PROJECT_ID.to_string());
            active_conversation_id.set(conv_id.clone());

            let mut msgs: Vec<UiMessage> = {
                load_messages_from_db(&conv_id).into_iter().map(UiMessage::Static).collect()
            };
            if msgs.is_empty() && !conv_id.is_empty() {
                tracing::warn!("load_messages_from_db returned empty for conv={}", conv_id);
            }
            if let Some(streaming) = ss.lock().unwrap_or_else(|e| e.into_inner()).get(&conv_id) {
                msgs.push(streaming.clone());
            }
            messages.set(msgs);
            if let Ok(conn) = crate::db::get_db().lock() {
                crate::db::metadata::project::touch(&conn, &proj_id).ok();
            }
        }
    };

    let on_delete_conversation = move |conv_id: String| {
        let proj_id = active_project_id.read().clone()
            .unwrap_or_else(|| crate::db::DEFAULT_PROJECT_ID.to_string());
        if *active_conversation_id.read() == conv_id {
            active_conversation_id.set(String::new());
            messages.set(Vec::new());
            streaming_segments.set(Vec::new());
            active_tool_calls.set(Vec::new());
        }
        if let Ok(conn) = crate::db::get_db().lock() {
            crate::db::metadata::message::delete_by_conversation(&conn, &conv_id).ok();
            crate::db::metadata::conversation::delete(&conn, &conv_id).ok();
        }
        {
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == proj_id) {
                proj.conversations.retain(|c| c.id != conv_id);
            }
        }
        AgentManager::get().lock().unwrap().remove(&conv_id);
        let curr = sidebar_view.read().clone();
        sidebar_view.set(curr);
    };

    let on_delete_project = move |project_id: String| {
        if project_id == crate::db::DEFAULT_PROJECT_ID { return; }
        let is_current = active_project_id.read().as_deref() == Some(&project_id);
        if is_current {
            sidebar_view.set(SidebarView::ProjectList);
            active_project_id.set(None);
            messages.set(Vec::new());
            streaming_segments.set(Vec::new());
        }
        if let Ok(conn) = crate::db::get_db().lock() {
            let convs = crate::db::metadata::conversation::list_by_project(&conn, &project_id).unwrap_or_default();
            let mut mgr = AgentManager::get().lock().unwrap();
            for conv in &convs { mgr.remove(&conv.id); }
            crate::db::metadata::project::delete(&conn, &project_id).ok();
        }
        let conn = crate::db::get_db().lock().unwrap();
        let rows = crate::db::metadata::project::list(&conn).unwrap_or_default();
        drop(conn);
        projects.set(
            rows.into_iter().map(|r| Project {
                id: r.id, name: r.name, path: r.path,
                created_at: r.created_at, conversations: Vec::new(),
                indicator_color: r.indicator_color, last_activity_at: r.last_activity_at,
            }).collect(),
        );
    };

    let on_change_indicator_color = move |(project_id, color_key): (String, String)| {
        let conn = crate::db::get_db().lock().unwrap();
        if let Some(mut row) = crate::db::metadata::project::get(&conn, &project_id).unwrap_or(None) {
            row.indicator_color = color_key;
            crate::db::metadata::project::update(&conn, &row).ok();
        }
        let rows = crate::db::metadata::project::list(&conn).unwrap_or_default();
        drop(conn);
        projects.set(
            rows.into_iter().map(|r| Project {
                id: r.id, name: r.name, path: r.path,
                created_at: r.created_at, conversations: Vec::new(),
                indicator_color: r.indicator_color, last_activity_at: r.last_activity_at,
            }).collect(),
        );
    };

    rsx! {
        style { {include_str!("style.css")} }
        div {
            class: "app-container",
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
                on_open_settings,
            }
            div {
                class: "main-area",
                if sidebar_view() == SidebarView::Settings {
                    SettingsPanel {
                        on_change: move |_| {
                            init_agent_from_first_instance();
                        },
                    }
                } else {
                    ChatPanel {
                        messages,
                        tick,
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
                InputBar {
                    is_streaming,
                    action_mode,
                    agent_mode,
                    on_cancel: move |_| {
                        if let Some(ref token) = *cancel_token.read() {
                            token.cancel();
                        }
                    },
                    on_send: {
                        let cache = AgentManager::get();
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
                            messages.write().push(UiMessage::Static(user_msg.clone()));

                            // 对话 ID
                            let cid = if conv_id.is_empty() {
                                // 新对话：由 init_or_get 生成 id 并创建 DB 行
                                let mut mgr = AgentManager::get().lock().unwrap();
                                let new_cid = mgr.init_or_get(
                                    None,
                                    &main_agent_prompt(),
                                    Some(pid.clone()),
                                ).unwrap_or_else(|_| String::new());
                                drop(mgr);
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
                                conv_id
                            };

                 

                            // DB 更新对话标题 + 项目活跃时间
                            if let Ok(conn) = crate::db::get_db().lock() {
                                conn.execute(
                                    "UPDATE conversations SET title = ?1 WHERE id = ?2 and (title = '' or title is null)",
                                    rusqlite::params![crate::model::title_from_messages(&[user_msg.clone()]), cid],
                                ).ok();
                                crate::db::metadata::project::touch(&conn, &pid).ok();
                            }
                            // 刷新 signal（标题、轮数、last_activity_at 同步）
                            if let Ok(conn) = crate::db::get_db().lock() {
                                let convs = crate::db::metadata::conversation::list_by_project(&conn, &pid).unwrap_or_default();
                                let mut p = projs.write();
                                if let Some(proj) = p.iter_mut().find(|pr| pr.id == pid) {
                                    proj.conversations = convs.into_iter().map(|c| Conversation {
                                        id: c.id, title: c.title, messages: Vec::new(),
                                        updated_at: c.updated_at, message_count: c.message_count as usize,
                                    }).collect();
                                    proj.last_activity_at = Some(chrono::Local::now());
                                }
                                drop(p);
                            }
                            streaming_project_id.set(Some(pid.clone()));

                            
                            let cur_action_mode = action_mode();
                            let cur_agent_mode = agent_mode();
                            let bridge_cancel = CancellationToken::new();
                            cancel_token.set(Some(bridge_cancel.clone()));

                            let ss = streaming_states.clone();
                            let cid2 = cid.clone();
                            let streaming_proj_sig = streaming_project_id;

                            spawn(async move {
                                crate::ui::bridge::run_agent_loop(
                                    input,
                                    cur_action_mode,
                                    cur_agent_mode,
                                    messages,
                                    is_streaming,
                                    streaming_proj_sig,
                                    bridge_cancel,
                                    cid2.clone(),
                                    ss,
                                    tick,
                                    err_sig,
                                )
                                .await;
                            });
                        }
                    },
                }
            }
            }

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
