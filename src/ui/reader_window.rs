//! 独立阅读窗口：主窗口点书后在这里打开/复用阅读器窗口。

use std::sync::{Mutex, OnceLock};

use dioxus::desktop::use_window;
use dioxus::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::ui::components::error::ErrorSignal;
use crate::ui::components::reader_panel::ReaderPanel;

/// 打开阅读器的请求：书 + 可选来源计划（书旁对话为全局书聊，计划仅用于上下文展示）。
#[derive(Clone, Debug)]
pub struct OpenBookRequest {
    pub book_id: String,
    pub project_id: Option<String>,
}

/// 主窗口 → 阅读窗口的“打开书”通道。
static READER_TX: OnceLock<Mutex<Option<UnboundedSender<OpenBookRequest>>>> = OnceLock::new();
/// 阅读窗口首启时取走的接收端（每开一次窗口重新放一个）。
static READER_RX: OnceLock<Mutex<Option<UnboundedReceiver<OpenBookRequest>>>> = OnceLock::new();

fn tx_slot() -> &'static Mutex<Option<UnboundedSender<OpenBookRequest>>> {
    READER_TX.get_or_init(|| Mutex::new(None))
}

fn rx_slot() -> &'static Mutex<Option<UnboundedReceiver<OpenBookRequest>>> {
    READER_RX.get_or_init(|| Mutex::new(None))
}

/// 打开一本书：阅读窗口已存在则直接切换，否则新建窗口。
pub fn open(book_id: String) {
    open_with_project(book_id, None);
}

/// 从学习计划打开一本书（携带来源计划 id）。
pub fn open_with_project(book_id: String, project_id: Option<String>) {
    if let Some(tx) = tx_slot().lock().ok().and_then(|g| g.clone())
        && tx
            .send(OpenBookRequest {
                book_id: book_id.clone(),
                project_id: project_id.clone(),
            })
            .is_ok()
    {
        return;
    }

    let (tx, rx) = unbounded_channel();
    if let Ok(mut g) = tx_slot().lock() {
        *g = Some(tx.clone());
    }
    if let Ok(mut g) = rx_slot().lock() {
        *g = Some(rx);
    }

    let dom = VirtualDom::new(ReaderWindowRoot);
    let cfg = dioxus::desktop::Config::new().with_window(
        dioxus::desktop::WindowBuilder::new()
            .with_title("UeberNeon")
            .with_inner_size(dioxus::desktop::LogicalSize::new(1280.0, 840.0))
            .with_min_inner_size(dioxus::desktop::LogicalSize::new(960.0, 640.0)),
    );
    // 窗口生命周期由 dioxus 的共享上下文持有，drop pending 不影响窗口存活。
    let _pending = dioxus::desktop::window().new_window(dom, cfg);
    let _ = tx.send(OpenBookRequest {
        book_id,
        project_id,
    });
}

/// 阅读窗口根组件：订阅主窗口发来的书 id，逐本替换内容。
#[component]
pub fn ReaderWindowRoot() -> Element {
    let book_id = use_signal(String::new);
    let project_id = use_signal(|| Option::<String>::None);
    let error_signal = use_signal(ErrorSignal::new);
    use_context_provider(|| error_signal);
    let desktop = use_window();

    use_effect(move || {
        let rx = rx_slot().lock().ok().and_then(|mut g| g.take());
        spawn(async move {
            let Some(mut rx) = rx else {
                return;
            };
            let mut book_id = book_id;
            let mut project_id = project_id;
            while let Some(id) = rx.recv().await {
                book_id.set(id.book_id);
                project_id.set(id.project_id);
            }
        });
    });

    let err_modal = error_signal.read().modal.clone();
    let err_banner = error_signal.read().banner.clone();

    rsx! {
        {
            let a = crate::settings::get().appearance;
            let fs = match a.font_size.as_str() {
                "xs" => "0.8125rem",
                "sm" => "0.875rem",
                "md" => "1rem",
                "lg" => "1.125rem",
                "xl" => "1.25rem",
                _ => "1rem",
            };
            let cf = match a.code_font.as_str() {
                "jetbrains-mono" => "\"JetBrains Mono\",\"SF Mono\",monospace",
                "geist-mono" => "\"Geist Mono\",\"SF Mono\",monospace",
                "ibm-plex-mono" => "\"IBM Plex Mono\",\"SF Mono\",monospace",
                "commit-mono" => "\"Commit Mono\",\"SF Mono\",monospace",
                _ => "\"JetBrains Mono\",\"SF Mono\",monospace",
            };
            let compact = if a.ui_density == "compact" {
                "--space-sm:0.5rem;--space-md:0.75rem;--space-lg:1rem;--space-xl:1.5rem;--space-2xl:2rem;"
            } else {
                ""
            };
            rsx! {
                style { ":root{{--text-base:{fs};--font-mono:{cf};{compact}}}" }
                style { {include_str!("components/style.css")} }
            }
        }
        div {
            class: "reader-window-root",
            if let Some(b) = err_banner {
                div { class: "reader-window-error", "{b.title}: {b.message}" }
            }
            if let Some(m) = err_modal {
                div { class: "reader-window-error is-modal", "{m.title}: {m.message}" }
            }
            if !book_id.read().is_empty() {
                ReaderPanel {
                    key: "{book_id}-{project_id:?}",
                    book_id: book_id(),
                    project_id: project_id(),
                    error_signal: error_signal,
                    on_back: move |_| {
                        desktop.close();
                    },
                }
            } else {
                div {
                    class: "reader-loading",
                    span { class: "reader-spinner" }
                    span { class: "reader-loading__label", "正在打开…" }
                }
            }
        }
    }
}
