use dioxus::desktop::use_window;
use dioxus::prelude::*;
use std::time::Duration;
use crate::agent::{ActionMode, AgentMode};

impl ActionMode {
    /// 用于 HTML option value 的键。
    pub fn as_key(self) -> &'static str {
        match self {
            ActionMode::Regular => "regular",
            ActionMode::Plan => "plan",
        }
    }

    /// 所有变体，供 UI 遍历。
    pub const ALL: &[ActionMode] = &[ActionMode::Regular, ActionMode::Plan];
}

impl AgentMode {
    /// 用于 HTML option value 的键。
    pub fn as_key(self) -> &'static str {
        match self {
            AgentMode::Cautious => "cautious",
            AgentMode::Ask => "ask",
            AgentMode::Auto => "auto",
            AgentMode::Unrestrained => "unrestrained",
        }
    }

    /// 所有变体，供 UI 遍历。
    pub const ALL: &[AgentMode] = &[
        AgentMode::Cautious,
        AgentMode::Ask,
        AgentMode::Auto,
        AgentMode::Unrestrained,
    ];
}

/// 底部输入栏 —— 模式选择 + 多行输入框 + 发送/取消按钮
#[component]
pub fn InputBar(
    is_streaming: Signal<bool>,
    action_mode: Signal<ActionMode>,
    agent_mode: Signal<AgentMode>,
    on_send: EventHandler<String>,
    on_cancel: EventHandler<()>,
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

            // ── 模式选择行 ──
            div {
                class: "mode-toggle-row",

                // ── 执行模式 ──
                div {
                    class: "mode-toggle-group",
                    label {
                        class: "mode-toggle-label",
                        "action"
                    }
                    div {
                        class: if running { "mode-pill-row is-disabled" } else { "mode-pill-row" },
                        for mode in ActionMode::ALL.iter() {
                            button {
                                class: if *mode == action_mode() { "mode-pill is-active" } else { "mode-pill" },
                                onclick: move |_| action_mode.set(*mode),
                                "{mode}"
                            }
                        }
                    }
                }

                // ── Agent 模式 ──
                div {
                    class: "mode-toggle-group",
                    label {
                        class: "mode-toggle-label",
                        "agent"
                    }
                    div {
                        class: if running { "mode-pill-row is-disabled" } else { "mode-pill-row" },
                        for mode in AgentMode::ALL.iter() {
                            button {
                                class: if *mode == agent_mode() {
                                    if *mode == AgentMode::Unrestrained { "mode-pill is-active is-danger" } else { "mode-pill is-active" }
                                } else {
                                    if *mode == AgentMode::Unrestrained { "mode-pill is-danger" } else { "mode-pill" }
                                },
                                onclick: move |_| agent_mode.set(*mode),
                                "{mode}"
                            }
                        }
                    }
                }
            }

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
                            on_cancel.call(());
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
