// ── 错误通知系统 ──
//
// 三种展示形式：
//   ErrorModal  — 居中模态弹窗（致命异常）
//   ErrorBanner — 顶部横幅（非致命警告）
//   ErrorToast  — 右下角通知（瞬时提示）
//
// 通过 ErrorSignal 集中管理：App 持有该 signal，子组件读取/写入。

use chrono::Local;
use dioxus::prelude::*;

/// 严重级别
#[derive(Clone, PartialEq)]
pub enum ErrorSeverity {
    /// 致命 — 模态弹窗阻断操作
    Fatal,
    /// 警告 — 顶部横幅，不阻断
    Warning,
    /// 提示 — 右下角 Toast，自动消失
    Info,
}

/// 错误来源
#[derive(Clone, PartialEq)]
pub enum ErrorSource {
    Agent,
    Startup,
    Validation,
    General,
}

/// 一条错误信息
#[derive(Clone, PartialEq)]
pub struct ErrorInfo {
    pub code: String,
    pub title: String,
    pub message: String,
    pub detail: Option<String>,
    pub severity: ErrorSeverity,
    pub source: ErrorSource,
    pub timestamp: String,
}

impl ErrorInfo {
    pub fn new(
        code: impl Into<String>,
        title: impl Into<String>,
        message: impl Into<String>,
        severity: ErrorSeverity,
        source: ErrorSource,
    ) -> Self {
        Self {
            code: code.into(),
            title: title.into(),
            message: message.into(),
            detail: None,
            severity,
            source,
            timestamp: Local::now().format("%H:%M:%S").to_string(),
        }
    }

    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }
}

// ═══════════════════════════════════════════════
//  ErrorModal — 居中模态弹窗
// ═══════════════════════════════════════════════

#[component]
pub fn ErrorModal(error: ErrorInfo, on_dismiss: EventHandler<()>) -> Element {
    let mut detail_open = use_signal(|| false);
    let has_detail = error.detail.is_some();

    let on_keydown = {
        let dismiss = on_dismiss.clone();
        move |evt: KeyboardEvent| {
            if evt.key() == Key::Escape {
                dismiss.call(());
            }
        }
    };

    rsx! {
        div {
            class: "error-modal-backdrop",
            tabindex: -1,
            onkeydown: on_keydown,
            onclick: move |_| on_dismiss.call(()),

            div {
                class: "error-modal",
                onclick: |evt| evt.stop_propagation(),
                role: "alertdialog",
                aria_labelledby: "error-modal-title",

                div { class: "error-modal-accent-bar" }

                div { class: "error-modal-body",
                    div { class: "error-modal-meta",
                        span { class: "error-modal-icon", "⚠" }
                        span { class: "error-modal-code", "{error.code}" }
                        span { class: "error-modal-time", "{error.timestamp}" }
                    }

                    h2 {
                        class: "error-modal-title",
                        id: "error-modal-title",
                        "{error.title}"
                    }

                    p { class: "error-modal-message", "{error.message}" }

                    if has_detail {
                        button {
                            class: "error-modal-detail-toggle",
                            onclick: move |_| detail_open.set(!detail_open()),
                            aria_expanded: "{detail_open()}",
                            span { class: "error-modal-toggle-icon",
                                if detail_open() { "▾" } else { "▸" }
                            }
                            " 技术详情"
                        }
                        if detail_open() {
                            {
                                let detail_text = error.detail.as_deref().unwrap_or("");
                                rsx! { pre { class: "error-modal-detail", "{detail_text}" } }
                            }
                        }
                    }
                }

                div { class: "error-modal-actions",
                    button {
                        class: "error-modal-btn error-modal-btn--primary",
                        autofocus: true,
                        onclick: move |_| on_dismiss.call(()),
                        "确认"
                    }
                }
            }
        }
    }
}

// ═══════════════════════════════════════════════
//  ErrorBanner — 顶部横幅
// ═══════════════════════════════════════════════

#[component]
pub fn ErrorBanner(error: ErrorInfo, on_dismiss: EventHandler<()>) -> Element {
    let warn = error.severity == ErrorSeverity::Warning;
    let label = match error.severity {
        ErrorSeverity::Fatal => "ERROR",
        ErrorSeverity::Warning => "WARNING",
        ErrorSeverity::Info => "INFO",
    };

    rsx! {
        div {
            class: if warn { "error-banner error-banner--warn" } else { "error-banner" },
            role: "alert",

            span { class: "error-banner-label", "{label}" }
            span { class: "error-banner-code", "[{error.code}]" }
            span { class: "error-banner-text", "{error.title}: {error.message}" }

            button {
                class: "error-banner-close",
                aria_label: "关闭",
                onclick: move |_| on_dismiss.call(()),
                "×"
            }
        }
    }
}

// ═══════════════════════════════════════════════
//  ErrorToast — 右下角通知
// ═══════════════════════════════════════════════

#[component]
pub fn ErrorToast(error: ErrorInfo, on_dismiss: EventHandler<()>) -> Element {
    rsx! {
        div {
            class: "error-toast",
            role: "status",
            aria_live: "polite",

            span { class: "error-toast-dot" }
            div { class: "error-toast-body",
                span { class: "error-toast-title", "{error.title}" }
                span { class: "error-toast-msg", "{error.message}" }
            }
            button {
                class: "error-toast-close",
                aria_label: "关闭",
                onclick: move |_| on_dismiss.call(()),
                "×"
            }
        }
    }
}

// ═══════════════════════════════════════════════
//  ErrorSignal — 集中管理
// ═══════════════════════════════════════════════

#[derive(Clone, PartialEq)]
pub struct ErrorSignal {
    pub modal: Option<ErrorInfo>,
    pub banner: Option<ErrorInfo>,
    pub toasts: Vec<ErrorInfo>,
}

impl ErrorSignal {
    pub fn new() -> Self {
        Self { modal: None, banner: None, toasts: Vec::new() }
    }

    pub fn push(&mut self, error: ErrorInfo) {
        match error.severity {
            ErrorSeverity::Fatal => self.modal = Some(error),
            ErrorSeverity::Warning => self.banner = Some(error),
            ErrorSeverity::Info => self.toasts.push(error),
        }
    }

    pub fn dismiss_modal(&mut self) { self.modal = None; }
    pub fn dismiss_banner(&mut self) { self.banner = None; }
    pub fn dismiss_toast(&mut self, index: usize) {
        if index < self.toasts.len() {
            self.toasts.remove(index);
        }
    }
}
