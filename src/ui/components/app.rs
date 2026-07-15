use dioxus::prelude::*;

use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::input_bar::InputBar;
use crate::ui::components::sidebar::Sidebar;
use crate::ui::state::*;

/// Markdown 转 HTML 辅助函数
fn markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new(md);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}

#[component]
pub fn App() -> Element {
    let mut conversations = use_signal(|| {
        vec![Conversation {
            id: "default".into(),
            title: "新对话".into(),
            messages: vec![],
        }]
    });
    let mut active_conversation_id = use_signal(|| "default".to_string());
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
        max_tokens: 8192,
        agent_mode: "ask".into(),
    });

    rsx! {
        style { {include_str!("style.css")} }

        div {
            class: "app-container",

            Sidebar {
                conversations,
                active_conversation_id,
                on_new_conversation: move |_| {
                    let id = format!("conv-{}", chrono::Local::now().timestamp_millis());
                    conversations.write().push(Conversation {
                        id: id.clone(),
                        title: "新对话".into(),
                        messages: vec![],
                    });
                    active_conversation_id.set(id);
                    messages.set(vec![]);
                    active_tool_calls.set(vec![]);
                },
                on_select_conversation: move |conv_id: String| {
                    active_conversation_id.set(conv_id.clone());
                    if let Some(conv) = conversations
                        .read()
                        .iter()
                        .find(|c| c.id == conv_id)
                    {
                        messages.set(conv.messages.clone());
                    }
                    active_tool_calls.set(vec![]);
                },
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
                    messages,
                    is_streaming,
                    on_send: move |input: String| {
                        let config_val = config.read().clone();
                        spawn(async move {
                            crate::ui::bridge::run_agent_loop(
                                input,
                                config_val,
                                messages,
                                streaming_content,
                                is_streaming,
                                active_tool_calls,
                            )
                            .await;
                        });
                    },
                }
            }
        }
    }
}
