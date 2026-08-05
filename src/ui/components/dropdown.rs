// ── 自定义下拉框 ──
// 完全自绘的 dropdown，替代原生 <select>。
// 弹出面板使用与 context-menu 一致的设计语言。
// 支持可搜索模式（searchable=true 时面板顶部显示搜索输入框）。
// 面板自动判断上下方向：下方空间不足时向上弹出（onmount 中检测，同一帧调整）。

use dioxus::prelude::*;
use std::time::Duration;

#[derive(Clone, PartialEq)]
pub struct DropdownOption {
    pub value: String,
    pub label: String,
}

/// 自绘下拉框
#[component]
pub fn Dropdown(
    value: String,
    onchange: EventHandler<String>,
    options: Vec<DropdownOption>,
    #[props(optional)] placeholder: Option<String>,
    #[props(optional)] searchable: Option<bool>,
) -> Element {
    let mut is_open = use_signal(|| false);
    let mut search_query = use_signal(String::new);
    let desktop = dioxus::desktop::use_window();

    let searchable = searchable.unwrap_or(false);

    let selected_label = options
        .iter()
        .find(|o| o.value == value)
        .map(|o| o.label.as_str())
        .or_else(|| placeholder.as_deref())
        .unwrap_or("");

    let has_value = options.iter().any(|o| o.value == value);

    let filtered_options: Vec<&DropdownOption> = if searchable && !search_query.read().is_empty() {
        let q = search_query.read().to_lowercase();
        options
            .iter()
            .filter(|o| o.label.to_lowercase().contains(&q))
            .collect()
    } else {
        options.iter().collect()
    };

    rsx! {
        div { class: "dropdown",
            button {
                class: "dropdown-trigger",
                onclick: move |_| {
                    is_open.set(!is_open());
                    if !is_open() {
                        search_query.set(String::new());
                    }
                },
                span {
                    class: if has_value { "dropdown-trigger-label" } else { "dropdown-trigger-label dropdown-trigger-placeholder" },
                    "{selected_label}"
                }
                span { class: "dropdown-arrow" }
            }

            if is_open() {
                div {
                    class: "dropdown-backdrop",
                    onclick: move |_| {
                        is_open.set(false);
                        search_query.set(String::new());
                    },
                }
                div {
                    // 默认向下，onmount 中检测空间并调整
                    class: "dropdown-panel",
                    onclick: move |evt| evt.stop_propagation(),
                    onmount: move |_| {
                        let w2 = desktop.clone();
                        async move {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            let _ = w2.webview.evaluate_script(
                                r#"
                                (function() {
                                    var panel = document.querySelector('.dropdown-panel:last-of-type');
                                    var btn = document.querySelector('.dropdown-trigger');
                                    if (!panel || !btn) return;
                                    var rect = btn.getBoundingClientRect();
                                    var below = window.innerHeight - rect.bottom;
                                    var above = rect.top;
                                    if (below < 280 && above > below) {
                                        panel.classList.add('dropdown-panel--up');
                                    }
                                })();
                                "#,
                            );
                        }
                    },

                    if searchable {
                        div { class: "dropdown-search-wrap",
                            input {
                                class: "dropdown-search-input",
                                r#type: "text",
                                placeholder: "search...",
                                value: "{search_query}",
                                oninput: move |evt| search_query.set(evt.value()),
                                onmount: move |_| async move {
                                    let _ = tokio::time::sleep(Duration::from_millis(10)).await;
                                },
                            }
                        }
                    }

                    if filtered_options.is_empty() {
                        div { class: "dropdown-empty", "no results" }
                    } else {
                        for opt in filtered_options {
                            {
                                let is_sel = opt.value == value;
                                let val = opt.value.clone();
                                rsx! {
                                    div {
                                        class: if is_sel { "dropdown-option is-selected" } else { "dropdown-option" },
                                        onclick: move |_| {
                                            onchange.call(val.clone());
                                            is_open.set(false);
                                            search_query.set(String::new());
                                        },
                                        span { class: "dropdown-option-label", "{opt.label}" }
                                        if is_sel {
                                            span { class: "dropdown-option-check", "✓" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
