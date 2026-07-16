use dioxus::prelude::*;
use std::sync::{Arc, Mutex};

use crate::agent::{ActionMode, AgentMode};

use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::input_bar::InputBar;
use crate::ui::components::sidebar::Sidebar;
use crate::ui::state::*;
use crate::ui::store;

/// Markdown 转 HTML 辅助函数
fn markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(
        md,
        pulldown_cmark::Options::ENABLE_TABLES,
    );
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

/// 从指定项目的对话列表中找出一条对话的消息
fn load_messages_for_conversation(
    projects: &[Project],
    project_id: &str,
    conv_id: &str,
) -> Vec<ChatMessage> {
    projects
        .iter()
        .find(|p| p.id == project_id)
        .and_then(|p| p.conversations.iter().find(|c| c.id == conv_id))
        .map(|c| c.messages.clone())
        .unwrap_or_default()
}

#[component]
pub fn App() -> Element {
    // ── 项目状态（持久化）──
    let mut projects = use_signal(|| {
        let mut p = store::load_projects();
        store::ensure_default_project(&mut p);
        p
    });
    let mut active_project_id = use_signal(|| Option::<String>::None);
    let mut sidebar_view = use_signal(|| SidebarView::ProjectList);

    // ── 当前对话状态 ──
    let mut active_conversation_id = use_signal(|| String::new());
    let mut messages = use_signal(Vec::<ChatMessage>::new);
    let mut streaming_segments = use_signal(Vec::<StreamSegment>::new);
    let mut is_streaming = use_signal(|| false);
    let mut streaming_project_id = use_signal(|| Option::<String>::None);
    let mut active_tool_calls = use_signal(Vec::<ToolCallRecord>::new);
    let mut action_mode = use_signal(|| ActionMode::Regular);
    let mut agent_mode = use_signal(|| AgentMode::Ask);
    let mut pending_approval = use_signal(|| Option::<PendingApproval>::None);
    let approval_responder: Signal<Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>> = use_signal(|| Arc::new(Mutex::new(None)));
    let config = use_signal(|| AppConfig {
        model: std::env::var("OPENAI_MODEL")
            .unwrap_or_else(|_| "deepseek-chat".into()),
        base_url: std::env::var("OPENAI_BASE_URL")
            .unwrap_or_else(|_| "https://api.deepseek.com".into()),
        api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
        temperature: 0.7,
        max_tokens: 4096,
        agent_mode: "ask".into(),
    });


    // ── 事件处理 ──

    /// 选择项目：加载该项目的对话
    let mut on_select_project = move |project_id: String| {
        // 设置当前项目
        active_project_id.set(Some(project_id.clone()));
        sidebar_view.set(SidebarView::ConversationList(project_id.clone()));

        // 如果有对话，选中第一个
        let projs = projects.read();
        if let Some(proj) = projs.iter().find(|p| p.id == project_id) {
            if let Some(first) = proj.conversations.first() {
                let cid = first.id.clone();
                drop(projs);
                active_conversation_id.set(cid.clone());
                let msgs = load_messages_for_conversation(&projects.read(), &project_id, &cid);
                messages.set(msgs);
                return;
            }
        }
        // 没有对话，清空消息
        active_conversation_id.set(String::new());
        messages.set(Vec::new());
        streaming_segments.set(Vec::new());
    };

    /// 返回项目列表
    let on_back_to_projects = move |_| {
        sidebar_view.set(SidebarView::ProjectList);
        active_project_id.set(None);
        active_conversation_id.set(String::new());
        messages.set(Vec::new());
        streaming_segments.set(Vec::new());
    };

    /// 新建项目
    let on_new_project = move |(name, path): (String, String)| {
        let id = format!("proj-{}", chrono::Local::now().timestamp_millis());
        let project = Project {
            id: id.clone(),
            name,
            path,
            created_at: chrono::Local::now(),
            conversations: Vec::new(),
            indicator_color: String::new(),
            last_activity_at: None,
        };
        projects.write().push(project);
        store::save_projects_quiet(&projects.read());
        // 自动选中新项目
        on_select_project(id);
    };

    /// 新建对话
    let on_new_conversation = move |_| {
        let current_proj = active_project_id.read().clone();
        let proj_id = match current_proj {
            Some(id) => id,
            None => {
                let def_id = store::DEFAULT_PROJECT_ID.to_string();
                active_project_id.set(Some(def_id.clone()));
                sidebar_view.set(SidebarView::ConversationList(def_id.clone()));
                def_id
            }
        };

        let conv_id = format!("conv-{}", chrono::Local::now().timestamp_millis());
        let now = chrono::Local::now();
        let conversation = Conversation {
            id: conv_id.clone(),
            title: String::new(),
            messages: Vec::new(),
            updated_at: now,
        };

        {
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == proj_id) {
                proj.conversations.push(conversation);
                proj.last_activity_at = Some(now);
            }
        }
        store::save_projects_quiet(&projects.read());

        active_conversation_id.set(conv_id);
        messages.set(Vec::new());
        streaming_segments.set(Vec::new());
        active_tool_calls.set(Vec::new());
    };

    /// 选择对话
    let on_select_conversation = move |conv_id: String| {
        let proj_id = active_project_id.read().clone()
            .unwrap_or_else(|| store::DEFAULT_PROJECT_ID.to_string());

        active_conversation_id.set(conv_id.clone());
        let msgs = load_messages_for_conversation(&projects.read(), &proj_id, &conv_id);
        messages.set(msgs);
        streaming_segments.set(Vec::new());
        active_tool_calls.set(Vec::new());
        // 更新项目活跃时间
        {
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == proj_id) {
                proj.last_activity_at = Some(chrono::Local::now());
            }
        }
        store::save_projects_quiet(&projects.read());
    };

    /// 删除对话
    let on_delete_conversation = move |conv_id: String| {
        let proj_id = active_project_id.read().clone()
            .unwrap_or_else(|| store::DEFAULT_PROJECT_ID.to_string());

        // 如果正在查看该对话，清空
        if *active_conversation_id.read() == conv_id {
            active_conversation_id.set(String::new());
            messages.set(Vec::new());
            streaming_segments.set(Vec::new());
            active_tool_calls.set(Vec::new());
        }

        {
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == proj_id) {
                proj.conversations.retain(|c| c.id != conv_id);
            }
        }
        store::save_projects_quiet(&projects.read());
        // 触发重渲染
        let curr = sidebar_view.read().clone();
        sidebar_view.set(curr);
    };

    /// 删除项目
    let on_delete_project = move |project_id: String| {
        // 默认项目不允许删除（安全检查）
        if project_id == store::DEFAULT_PROJECT_ID {
            return;
        }

        // 如果正在查看该项目，返回项目列表
        let is_current = active_project_id.read().as_deref() == Some(&project_id);
        if is_current {
            sidebar_view.set(SidebarView::ProjectList);
            active_project_id.set(None);
            messages.set(Vec::new());
            streaming_segments.set(Vec::new());
        }

        {
            let mut projs = projects.write();
            projs.retain(|p| p.id != project_id);
        }
        store::save_projects_quiet(&projects.read());
    };

    /// 更改项目 indicator 颜色
    let on_change_indicator_color = move |(project_id, color_key): (String, String)| {
        let mut projs = projects.write();
        if let Some(proj) = projs.iter_mut().find(|p| p.id == project_id) {
            proj.indicator_color = color_key;
        }
        drop(projs);
        store::save_projects_quiet(&projects.read());
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
            }

            div {
                class: "main-area",

                ChatPanel {
                    messages,
                    streaming_segments,
                    is_streaming,
                    active_tool_calls,
                    markdown_to_html,
                    on_approve: {
                        let resp = approval_responder;
                        move |(allowed,): (bool,)| {
                            if let Some(tx) = resp.read().lock().unwrap().take() {
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
                    on_send: move |input: String| {
                        // 将用户消息写入 messages signal（UI 显示）
                        messages.write().push(ChatMessage {
                            role: Role::User,
                            content: input.clone(),
                            timestamp: chrono::Local::now(),
                            tool_calls: vec![],
                            reasoning: String::new(),
                            segments: Vec::new(),
                        });

                        // 保存用户消息到当前对话（持久化）
                        // 如果未选中项目/对话，自动在默认项目创建新对话
                        {
                            let proj_id = active_project_id.read().clone();
                            let conv_id = active_conversation_id.read().clone();

                            // 确保有项目
                            let pid = match proj_id {
                                Some(ref id) => id.clone(),
                                None => {
                                    let default_id = store::DEFAULT_PROJECT_ID.to_string();
                                    active_project_id.set(Some(default_id.clone()));
                                    sidebar_view.set(SidebarView::ConversationList(default_id.clone()));
                                    default_id
                                }
                            };

                            // 确保有对话，没有则新建
                            let cid = if conv_id.is_empty() {
                                let new_cid = format!("conv-{}", chrono::Local::now().timestamp_millis());
                                let now = chrono::Local::now();
                                let conversation = Conversation {
                                    id: new_cid.clone(),
                                    title: String::new(),
                                    messages: Vec::new(),
                                    updated_at: now,
                                };
                                {
                                    let mut projs = projects.write();
                                    if let Some(proj) = projs.iter_mut().find(|p| p.id == pid) {
                                        proj.conversations.push(conversation);
                                    }
                                }
                                store::save_projects_quiet(&projects.read());
                                active_conversation_id.set(new_cid.clone());
                                new_cid
                            } else {
                                conv_id
                            };

                            let is_first_msg = {
                                let msgs = messages.read();
                                msgs.len() == 1
                            };

                            let mut projs = projects.write();
                            if let Some(proj) = projs.iter_mut().find(|p| p.id == pid) {
                                if let Some(conv) = proj.conversations.iter_mut().find(|c| c.id == cid) {
                                    conv.messages.push(ChatMessage {
                                        role: Role::User,
                                        content: input.clone(),
                                        timestamp: chrono::Local::now(),
                                        tool_calls: vec![],
                                        reasoning: String::new(),
                                        segments: Vec::new(),
                                    });
                                    conv.updated_at = chrono::Local::now();
                                    proj.last_activity_at = Some(chrono::Local::now());
                                    // 首条消息：自动设置对话标题
                                    if is_first_msg && conv.title.is_empty() {
                                        let title = crate::ui::state::title_from_messages(&conv.messages);
                                        conv.title = title;
                                    }
                                }
                            }
                            streaming_project_id.set(Some(pid.clone()));
                        }

                        let projects_sig = projects;
                        let active_project_id_sig = active_project_id;
                        let active_conversation_id_sig = active_conversation_id;
                        let streaming_project_id_sig = streaming_project_id;
                        let config_val = config.read().clone();
                        let cur_action_mode = action_mode();
                        let cur_agent_mode = agent_mode();
                        let bridge_responder = approval_responder.read().clone();
                        let bridge_pending = pending_approval;

                        spawn(async move {
                            crate::ui::bridge::run_agent_loop(
                                input,
                                config_val,
                                cur_action_mode,
                                cur_agent_mode,
                                bridge_responder,
                                bridge_pending,
                                messages,
                                streaming_segments,
                                is_streaming,
                                active_tool_calls,
                                projects_sig,
                                active_project_id_sig,
                                active_conversation_id_sig,
                                streaming_project_id_sig,
                            )
                            .await;
                        });
                    },
                }
            }
        }
    }
}
