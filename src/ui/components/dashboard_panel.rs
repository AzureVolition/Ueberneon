// ── Dashboard Panel — 当前对话 Token 用量看板（右侧滑出） ──
// 数据来源：当前 ConversationRuntime 的 accumulated_usage，由 bridge 实时累加。

use dioxus::prelude::*;

/// 格式化大数字（k/m 后缀）
fn fmt_tokens(n: u32) -> String {
    let n = n as f64;
    if n >= 1_000_000.0 {
        format!("{:.1}m", n / 1_000_000.0)
    } else if n >= 1_000.0 {
        format!("{:.1}k", n / 1_000.0)
    } else {
        (n as u64).to_string()
    }
}

/// 单条分解条形图
#[component]
fn BreakdownBar(label: String, value: u32, total: u32, class: String) -> Element {
    let pct = if total > 0 { (value as f64 / total as f64) * 100.0 } else { 0.0 };
    rsx! {
        div { class: "dash-breakdown-row",
            span { class: "dash-breakdown-row__label", "{label}" }
            div { class: "dash-breakdown-row__track",
                div {
                    class: "dash-breakdown-row__fill {class}",
                    width: "{pct}%",
                }
            }
            span { class: "dash-breakdown-row__pct", "{pct:.0}%" }
            span { class: "dash-breakdown-row__val", "{fmt_tokens(value)}" }
        }
    }
}

/// DashboardPanel — 右侧滑出看板，展示当前对话 token 统计
#[component]
pub fn DashboardPanel(
    usage: crate::model::TokenUsageRecord,
    request_count: u64,
    context_window: u32,
    last_prompt_tokens: Option<u32>,
) -> Element {
    let mut is_open = use_signal(|| false);

    let total = usage.total_tokens;
    let cache_total = usage.cache_hit_tokens + usage.cache_miss_tokens;
    let cache_rate = if cache_total > 0 {
        (usage.cache_hit_tokens as f64 / cache_total as f64) * 100.0
    } else { 0.0 };
    let last_prompt = last_prompt_tokens.unwrap_or(0);
    let window_pct = if context_window > 0 {
        ((last_prompt as f64 / context_window as f64) * 100.0).min(100.0)
    } else { 0.0 };

    rsx! {
        // 触发按钮 — 内联 pill，显示请求计数
        button {
            class: {
                if is_open() { "dash-trigger dash-trigger--active" } else { "dash-trigger" }
            },
            title: "toggle usage dashboard",
            onclick: move |_| is_open.toggle(),
            span { class: "dash-trigger__label", "USAGE {request_count}" }
        }

        // 遮罩层 — 点击外部区域关闭面板
        if is_open() {
            div {
                class: "dash-backdrop",
                onclick: move |_| is_open.set(false),
            }
        }

        // 滑出面板
        div {
            class: {
                if is_open() { "dash-panel dash-panel--open" } else { "dash-panel" }
            },
            div { class: "dash-panel__header",
                span { class: "dash-panel__title", "Usage Dashboard" }
                button {
                    class: "dash-panel__close",
                    onclick: move |_| is_open.set(false),
                    "x"
                }
            }
            div { class: "dash-panel__scroll",
                if total == 0 {
                    div { class: "dash-card dash-card--single",
                        div { class: "dash-card__head",
                            span { class: "dash-card__label", "NO DATA" }
                        }
                        div { class: "dash-card__body",
                            div { class: "dash-empty", "Send a message to see token usage." }
                        }
                    }
                } else {
                    // 上下文窗口使用状态
                    div { class: "dash-card",
                        div { class: "dash-card__head",
                            span { class: "dash-card__label", "CONTEXT WINDOW" }
                            span { class: "dash-card__sub", "{fmt_tokens(last_prompt)} / {fmt_tokens(context_window)}" }
                        }
                        div { class: "dash-card__body",
                            div { class: "dash-window-track",
                                div {
                                    class: "dash-window-fill",
                                    width: "{window_pct}%",
                                }
                            }
                            div { class: "dash-window-pct", "{window_pct:.1}%" }
                        }
                    }
                    // 会话指标
                    div { class: "dash-card",
                        div { class: "dash-card__head",
                            span { class: "dash-card__label", "SESSION METRICS" }
                            span { class: "dash-card__sub", "current conversation" }
                        }
                        div { class: "dash-card__body dash-metrics",
                            div { class: "dash-metric",
                                span { class: "dash-metric__value", "{request_count}" }
                                span { class: "dash-metric__label", "requests" }
                            }
                            div { class: "dash-metric",
                                span { class: "dash-metric__value", "{fmt_tokens(total)}" }
                                span { class: "dash-metric__label", "total tokens" }
                            }
                            div { class: "dash-metric",
                                span { class: "dash-metric__value dash-metric__value--accent", "{cache_rate:.0}%" }
                                span { class: "dash-metric__label", "cache hit" }
                            }
                            div { class: "dash-metric",
                                span { class: "dash-metric__value", "{fmt_tokens(last_prompt_tokens.unwrap_or(0))}" }
                                span { class: "dash-metric__label", "last prompt" }
                            }
                        }
                    }
                    // Token 分解
                    div { class: "dash-card",
                        div { class: "dash-card__head",
                            span { class: "dash-card__label", "TOKEN BREAKDOWN" }
                            span { class: "dash-card__sub", "prompt / completion / reasoning" }
                        }
                        div { class: "dash-card__body",
                            BreakdownBar {
                                label: "prompt".to_string(),
                                value: usage.prompt_tokens,
                                total,
                                class: "dash-bar--prompt",
                            }
                            BreakdownBar {
                                label: "completion".to_string(),
                                value: usage.completion_tokens,
                                total,
                                class: "dash-bar--completion",
                            }
                            BreakdownBar {
                                label: "reasoning".to_string(),
                                value: usage.reasoning_tokens,
                                total,
                                class: "dash-bar--reasoning",
                            }
                            div { class: "dash-total-row",
                                span { class: "dash-total-row__label", "TOTAL" }
                                span { class: "dash-total-row__value", "{fmt_tokens(total)}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
