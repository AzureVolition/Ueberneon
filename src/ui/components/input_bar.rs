use dioxus::desktop::use_window;
use dioxus::prelude::*;
use std::time::Duration;

use crate::ui::state::*;

/// 底部输入栏 —— 多行输入框 + 发送/取消按钮
#[component]
pub fn InputBar(
    is_streaming: Signal<bool>,
    on_send: EventHandler<String>,
) -> Element {
    let mut input = use_signal(String::new);
    let mut idle_pulse = use_signal(|| false);
    let mut pulse_gen = use_signal(|| 0u64);
    let running = is_streaming();
    let desktop = use_window();
    let desktop_kb = desktop.clone();
    let desktop_btn = desktop.clone();

    let mut on_input = move |evt: FormEvent| {
        let val = evt.value();
        let is_empty = val.trim().is_empty();
        input.set(val);
        idle_pulse.set(false);
        let g = pulse_gen() + 1;
        pulse_gen.set(g);
        if !is_empty {
            let mut pulse = idle_pulse;
            let gen_sig = pulse_gen;
            spawn(async move {
                tokio::time::sleep(Duration::from_secs(10)).await;
                if gen_sig() == g {
                    pulse.set(true);
                }
            });
        }
    };

    rsx! {
        div {
            class: "input-bar",

            div {
                class: "input-row",

                textarea {
                    class: "input-textarea",
                    placeholder: "type your message... (⌘↵ to send)",
                    disabled: running,
                    rows: 2,
                    oninput: on_input,
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter
                            && (evt.modifiers().contains(Modifiers::META)
                                || evt.modifiers().contains(Modifiers::CONTROL))
                            && !evt.is_composing()
                        {
                            let text = input.read().trim().to_string();
                            if !text.is_empty() && !is_streaming() {
                                on_send.call(text);
                                input.set(String::new());
                                idle_pulse.set(false);
                                pulse_gen.set(pulse_gen() + 1);
                                let _ = desktop_kb.webview.evaluate_script(
                                    "document.querySelector('.input-textarea').value = ''",
                                );
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
                        class: if idle_pulse() && !input.read().is_empty() {
                            "btn btn-send btn-send-pulse"
                        } else {
                            "btn btn-send"
                        },
                        onclick: move |_| {
                            let text = input.read().trim().to_string();
                            if !text.is_empty() && !is_streaming() {
                                on_send.call(text);
                                input.set(String::new());
                                idle_pulse.set(false);
                                pulse_gen.set(pulse_gen() + 1);
                                let _ = desktop_btn.webview.evaluate_script(
                                    "document.querySelector('.input-textarea').value = ''",
                                );
                            }
                        },
                        "send"
                    }
                }
            }
        }
    }
}
