use dioxus::prelude::*;

use crate::ui::state::*;

/// 底部输入栏 —— 多行输入框 + 发送/取消按钮
#[component]
pub fn InputBar(
    messages: Signal<Vec<ChatMessage>>,
    is_streaming: Signal<bool>,
    on_send: EventHandler<String>,
) -> Element {
    let mut input = use_signal(String::new);
    let running = is_streaming();

    rsx! {
        div {
            class: "input-bar",

            div {
                class: "input-row",

                textarea {
                    class: "input-textarea",
                    value: "{input}",
                    placeholder: "type your message... (enter to send, shift+enter for new line)",
                    disabled: running,
                    rows: 2,
                    oninput: move |evt| input.set(evt.value()),
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter && !evt.modifiers().contains(Modifiers::SHIFT) {
                            let text = input.read().trim().to_string();
                            if !text.is_empty() && !is_streaming() {
                                messages.write().push(ChatMessage {
                                    role: Role::User,
                                    content: text.clone(),
                                    timestamp: chrono::Local::now(),
                                    tool_calls: vec![],
                                });
                                on_send.call(text);
                                input.set(String::new());
                            }
                        }
                    },
                }

                if running {
                    button {
                        class: "btn btn-cancel",
                        onclick: move |_| {
                            is_streaming.set(false);
                        },
                        "cancel"
                    }
                } else {
                    button {
                        class: "btn btn-send",
                        onclick: move |_| {
                            let text = input.read().trim().to_string();
                            if !text.is_empty() && !is_streaming() {
                                messages.write().push(ChatMessage {
                                    role: Role::User,
                                    content: text.clone(),
                                    timestamp: chrono::Local::now(),
                                    tool_calls: vec![],
                                });
                                on_send.call(text);
                                input.set(String::new());
                            }
                        },
                        "send"
                    }
                }
            }
        }
    }
}
