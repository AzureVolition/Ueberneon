use dioxus::prelude::*;

use crate::ui::state::{ChatMessage, Role, ToolCallRecord, ToolCallStatus};

/// 对话面板 —— 消息列表 + 流式输出 + 空状态
#[component]
pub fn ChatPanel(
    messages: Signal<Vec<ChatMessage>>,
    streaming_content: Signal<String>,
    is_streaming: Signal<bool>,
    active_tool_calls: Signal<Vec<ToolCallRecord>>,
    markdown_to_html: fn(&str) -> String,
) -> Element {
    let msgs = messages.read();
    let streaming = streaming_content.read();
    let running = is_streaming();
    let active_calls = active_tool_calls.read();

    rsx! {
        div {
            class: "chat-panel",

            if msgs.is_empty() && streaming.is_empty() {
                div {
                    class: "chat-empty",
                    span { class: "empty-eyebrow", "01 · CHAT" }
                    h2 {
                        dangerous_inner_html: "ready to <em>think</em> with you."
                    }
                    p { "start a conversation — type your message below." }
                }
            }

            {msgs.iter().enumerate().map(|(i, msg)| {
                let formatted_time = msg.timestamp.format("%H:%M:%S").to_string();
                let (role_label, role_class) = match msg.role {
                    Role::User => ("USER", "user-role"),
                    Role::Assistant => ("ASSISTANT", ""),
                    Role::System => ("SYSTEM", ""),
                };
                let bubble_class = match msg.role {
                    Role::User => "message-bubble message-user",
                    Role::Assistant => "message-bubble message-assistant",
                    Role::System => "message-bubble message-system",
                };
                let content_html = markdown_to_html(&msg.content);
                let tool_calls_html: Vec<_> = msg.tool_calls.iter().map(|call| {
                    let status_text = match call.status {
                        ToolCallStatus::Running => "running",
                        ToolCallStatus::Success => "success",
                        ToolCallStatus::Failed(_) => "failed",
                    };
                    let result_html = call.result.as_ref().map(|r| {
                        format!("<pre class=\"tool-call-result\">{}</pre>", html_escape(r))
                    }).unwrap_or_default();
                    format!(
                        "<div class=\"tool-call-card\"><div class=\"tool-call-header\"><span class=\"tool-call-name\">{}</span><span class=\"tool-call-status\">{}</span></div>{}</div>",
                        html_escape(&call.tool_name), status_text, result_html
                    )
                }).collect();

                let tool_calls_section = if tool_calls_html.is_empty() {
                    String::new()
                } else {
                    format!("<div class=\"tool-calls\">{}</div>", tool_calls_html.join(""))
                };

                let full_html = format!(
                    "<div class=\"message-header\"><span class=\"message-role {}\">{}</span><span class=\"message-time\">{}</span></div><div class=\"message-content\">{}</div>{}",
                    role_class, role_label, formatted_time, content_html, tool_calls_section
                );

                rsx! {
                    div {
                        key: "{i}",
                        class: bubble_class,
                        dangerous_inner_html: full_html,
                    }
                }
            })}

            {active_calls.iter().map(|call| {
                rsx! {
                    div {
                        key: "{call.tool_name}",
                        class: "message-bubble message-assistant",
                        div {
                            class: "tool-calls",
                            div {
                                class: "tool-call-card",
                                div {
                                    class: "tool-call-header",
                                    span {
                                        class: "tool-call-name",
                                        "{call.tool_name}"
                                    }
                                    span {
                                        class: "tool-call-status",
                                        match call.status {
                                            ToolCallStatus::Running => "running",
                                            ToolCallStatus::Success => "success",
                                            ToolCallStatus::Failed(_) => "failed",
                                        }
                                    }
                                }
                                if let Some(ref result) = call.result {
                                    pre {
                                        class: "tool-call-result",
                                        "{result}"
                                    }
                                }
                            }
                        }
                    }
                }
            })}

            if running && !streaming.is_empty() {
                div {
                    class: "message-bubble message-assistant streaming",
                    div {
                        class: "message-content",
                        dangerous_inner_html: markdown_to_html(&streaming),
                    }
                }
            }

            if running && streaming.is_empty() {
                div {
                    class: "message-bubble message-assistant thinking",
                    div {
                        class: "thinking-dots",
                        span { "." }
                        span { "." }
                        span { "." }
                    }
                }
            }
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
