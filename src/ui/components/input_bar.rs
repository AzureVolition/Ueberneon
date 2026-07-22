use crate::ui::components::app::ErrorSignal;
use dioxus::desktop::use_window;
use dioxus::prelude::*;
use std::time::Duration;
use crate::agent::{ActionMode, AgentMode};
use crate::db::metadata::agent_config::AgentConfigRow;
use crate::ui::components::dropdown::{Dropdown, DropdownOption};

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

/// 底部输入栏 —— 模式选择 + agent config 选择 + 多行输入框 + 发送/取消按钮
#[component]
pub fn InputBar(
    is_streaming: Signal<bool>,
    action_mode: Signal<ActionMode>,
    agent_mode: Signal<AgentMode>,
    /// 可选 — 可用 agent 配置列表
    agent_configs: Vec<AgentConfigRow>,
    /// 当前选中的 agent config id
    selected_agent_config_id: String,
    /// 切换 agent config 回调
    on_agent_config_change: EventHandler<String>,
    /// 切换 agent mode 回调（streaming 期间也可调用）
    on_agent_mode_change: EventHandler<AgentMode>,
    /// 是否禁用 config 选择（已有固定配置的对话）
    config_disabled: bool,
    /// 审批提示文本 — PlanPanel 点击"输入修改意见"后设置，
    /// InputBar 自动填入输入框并聚焦。
    approval_hint_text: Signal<Option<String>>,
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
    let _error_signal = use_context::<Signal<ErrorSignal>>();

    // 当 approval_hint_text 有值时，自动填入输入框
    use_effect(move || {
        let hint_opt = approval_hint_text.read();
        if let Some(ref hint) = *hint_opt {
            let val = hint.clone();
            drop(hint_opt);
            // 设置 Dioxus signal
            input.set(val.clone());
            // 同步设置 DOM（textarea 是 uncontrolled）
            let js = format!(
                "var el = document.querySelector('.input-textarea'); if(el) {{ el.value = {}; el.focus(); el.setSelectionRange(el.value.length, el.value.length); }}",
                serde_json::to_string(&val).unwrap_or_default()
            );
            let _ = desktop.webview.evaluate_script(&js);
            approval_hint_text.set(None);
        }
    });

    let on_input = move |evt: FormEvent| {
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

    // 构建 agent config 下拉选项
    let agent_options: Vec<DropdownOption> = if agent_configs.is_empty() {
        vec![DropdownOption {
            value: String::new(),
            label: "— no config —".into(),
        }]
    } else {
        agent_configs.iter().map(|cfg| {
            DropdownOption {
                value: cfg.id.clone(),
                label: format!("{} · {}", cfg.name, cfg.model),
            }
        }).collect()
    };

    let no_agent_configs = agent_configs.is_empty();

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
                        "agent mode"
                    }
                    div {
                        class: "mode-pill-row",
                        for mode in AgentMode::ALL.iter() {
                            button {
                                class: if *mode == agent_mode() {
                                    if *mode == AgentMode::Unrestrained { "mode-pill is-active is-danger" } else { "mode-pill is-active" }
                                } else {
                                    if *mode == AgentMode::Unrestrained { "mode-pill is-danger" } else { "mode-pill" }
                                },
                                onclick: move |_| on_agent_mode_change.call(*mode),
                                "{mode}"
                            }
                        }
                    }
                }

                // ── Agent config 选择 ──
                div {
                    class: "mode-toggle-group",
                    label {
                        class: "mode-toggle-label",
                        "agent config"
                    }
                    // 使用搜索版 Dropdown；没有可用配置时禁用交互
                    div {
                        class: if running || config_disabled { "agent-config-dropdown is-disabled" } else { "agent-config-dropdown" },
                        Dropdown {
                            value: selected_agent_config_id,
                            onchange: move |val: String| on_agent_config_change.call(val),
                            options: agent_options,
                            placeholder: if no_agent_configs { "— no config —" } else { "— select —" },
                            searchable: Some(true),
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
