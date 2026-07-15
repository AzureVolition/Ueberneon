use dioxus::prelude::*;

use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::input_bar::InputBar;
use crate::ui::components::sidebar::Sidebar;
use crate::ui::state::*;
use crate::ui::store;

/// Markdown 转 HTML 辅助函数
fn markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new(md);
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
    let mut streaming_content = use_signal(String::new);
    let mut is_streaming = use_signal(|| false);
    let mut active_tool_calls = use_signal(Vec::<ToolCallRecord>::new);
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
    };

    /// 返回项目列表
    let on_back_to_projects = move |_| {
        sidebar_view.set(SidebarView::ProjectList);
        active_project_id.set(None);
        messages.set(Vec::new());
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
        };
        projects.write().push(project);
        store::save_projects_quiet(&projects.read());
        // 自动选中新项目
        on_select_project(id);
    };

    /// 新建对话
    let on_new_conversation = move |_| {
        let proj_id = active_project_id.read().clone();
        let Some(ref proj_id) = proj_id else { return };

        let conv_id = format!("conv-{}", chrono::Local::now().timestamp_millis());
        let conversation = Conversation {
            id: conv_id.clone(),
            title: "新对话".into(),
            messages: Vec::new(),
        };

        {
            let mut projs = projects.write();
            if let Some(proj) = projs.iter_mut().find(|p| p.id == *proj_id) {
                proj.conversations.push(conversation);
            }
        }
        store::save_projects_quiet(&projects.read());

        active_conversation_id.set(conv_id);
        messages.set(Vec::new());
        active_tool_calls.set(Vec::new());
    };

    /// 选择对话
    let on_select_conversation = move |conv_id: String| {
        let proj_id = active_project_id.read().clone();
        let Some(ref proj_id) = proj_id else { return };

        active_conversation_id.set(conv_id.clone());
        let msgs = load_messages_for_conversation(&projects.read(), proj_id, &conv_id);
        messages.set(msgs);
        active_tool_calls.set(Vec::new());
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
                on_new_project,
                on_new_conversation,
                on_select_project,
                on_select_conversation,
                on_back_to_projects,
            }

            div {
                class: "main-area",

                ChatPanel {
                    messages,
                    streaming_content,
                    is_streaming,
                    active_tool_calls,
                    markdown_to_html,
                }

                InputBar {
                    is_streaming,
                    on_send: move |input: String| {
                        // 将用户消息写入 messages signal（UI 显示）
                        messages.write().push(ChatMessage {
                            role: Role::User,
                            content: input.clone(),
                            timestamp: chrono::Local::now(),
                            tool_calls: vec![],
                        });

                        // 保存用户消息到当前对话（持久化）
                        {
                            let proj_id = active_project_id.read().clone();
                            let conv_id = active_conversation_id.read().clone();
                            if let (Some(ref pid), cid) = (proj_id, conv_id) {
                                if !cid.is_empty() {
                                    let mut projs = projects.write();
                                    if let Some(proj) = projs.iter_mut().find(|p| p.id == *pid) {
                                        if let Some(conv) = proj.conversations.iter_mut().find(|c| c.id == cid) {
                                            conv.messages.push(ChatMessage {
                                                role: Role::User,
                                                content: input.clone(),
                                                timestamp: chrono::Local::now(),
                                                tool_calls: vec![],
                                            });
                                        }
                                    }
                                }
                            }
                        }

                        let projects_sig = projects;
                        let active_project_id_sig = active_project_id;
                        let active_conversation_id_sig = active_conversation_id;
                        let config_val = config.read().clone();

                        spawn(async move {
                            crate::ui::bridge::run_agent_loop(
                                input,
                                config_val,
                                messages,
                                streaming_content,
                                is_streaming,
                                active_tool_calls,
                                projects_sig,
                                active_project_id_sig,
                                active_conversation_id_sig,
                            )
                            .await;
                        });
                    },
                }
            }
        }
    }
}
