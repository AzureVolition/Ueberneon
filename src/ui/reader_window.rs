//! 独立阅读窗口：主窗口点书后在这里打开/复用阅读器窗口。
//!
//! 阅读窗口内以页签组织多本书：打开新书或从书内引用跳到另一本书时，
//! 如果该书尚未打开则新建页签；如果已经打开则切到已有页签。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use dioxus::desktop::use_window;
use dioxus::prelude::*;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

use crate::model::BookCitation;
use crate::ui::components::error::ErrorSignal;
use crate::ui::components::reader_panel::ReaderPanel;

/// 打开阅读器的请求：书 + 可选来源计划（书旁对话为全局书聊，计划仅用于上下文展示）。
#[derive(Clone, Debug)]
pub struct OpenBookRequest {
    pub book_id: String,
    pub project_id: Option<String>,
    pub citation: Option<crate::model::BookCitation>,
}

/// 阅读窗口中的一个页签。
#[derive(Clone, PartialEq)]
struct ReaderTab {
    id: String,
    book_id: String,
    project_id: Option<String>,
    citation: Option<BookCitation>,
    /// 同一页签再次收到待跳转引用时 +1，用于强制重挂载阅读器。
    revision: u64,
    title: String,
}

static TAB_COUNTER: AtomicU64 = AtomicU64::new(1);

fn next_tab_id() -> String {
    format!("reader-tab-{}", TAB_COUNTER.fetch_add(1, Ordering::Relaxed))
}

fn book_title(book_id: &str) -> String {
    let name = crate::db::with_db(|conn| crate::books::get(conn, book_id).ok().flatten())
        .map(|b| b.name)
        .unwrap_or_default();
    let name = name.trim();
    if name.is_empty() {
        book_id.to_string()
    } else {
        name.to_string()
    }
}

/// 打开/切换页签。同一本书复用已有页签；携带引用时更新该页签的待跳转引用。
fn open_request(
    mut tabs: Signal<Vec<ReaderTab>>,
    mut active_tab_id: Signal<String>,
    request: OpenBookRequest,
) {
    let existing_index = tabs
        .read()
        .iter()
        .position(|tab| tab.book_id == request.book_id);

    if let Some(index) = existing_index {
        let current_id = active_tab_id.read().clone();
        let tab_id = {
            let mut guard = tabs.write();
            let tab = &mut guard[index];
            if let Some(project_id) = request.project_id {
                if tab.project_id.as_deref() != Some(project_id.as_str()) {
                    tab.project_id = Some(project_id);
                    // 仅切换来源计划时不重放旧引用。
                    if request.citation.is_none() {
                        tab.citation = None;
                    }
                }
            }
            if let Some(citation) = request.citation {
                tab.citation = Some(citation);
                tab.revision += 1;
            }
            tab.id.clone()
        };

        if current_id != tab_id {
            clear_tab_citation(tabs, &current_id);
        }
        active_tab_id.set(tab_id);
        return;
    }

    let tab = ReaderTab {
        id: next_tab_id(),
        book_id: request.book_id.clone(),
        title: book_title(&request.book_id),
        project_id: request.project_id,
        citation: request.citation,
        revision: 0,
    };
    let current_id = active_tab_id.read().clone();
    tabs.write().push(tab.clone());
    if current_id != tab.id {
        clear_tab_citation(tabs, &current_id);
    }
    active_tab_id.set(tab.id);
}

/// 切走前清除上一页签里已消费/不再需要的待跳转引用，
/// 避免下次切回时重新挂载后又跳到旧位置。
fn clear_tab_citation(mut tabs: Signal<Vec<ReaderTab>>, tab_id: &str) {
    if tab_id.is_empty() {
        return;
    }
    if let Some(tab) = tabs.write().iter_mut().find(|tab| tab.id == tab_id) {
        tab.citation = None;
    }
}

/// 激活页签（点击页签时使用）。
fn activate_tab(tabs: Signal<Vec<ReaderTab>>, mut active_tab_id: Signal<String>, tab_id: String) {
    let current_id = active_tab_id.read().clone();
    if current_id == tab_id {
        return;
    }
    clear_tab_citation(tabs, &current_id);
    active_tab_id.set(tab_id);
}

/// 关闭页签；关闭当前页签后激活相邻页签，没有页签时关闭窗口。
fn close_tab(
    mut tabs: Signal<Vec<ReaderTab>>,
    mut active_tab_id: Signal<String>,
    desktop: dioxus::desktop::DesktopContext,
    tab_id: String,
) {
    let current_id = active_tab_id.read().clone();
    let removed_index = tabs.read().iter().position(|tab| tab.id == tab_id);
    let Some(removed_index) = removed_index else {
        return;
    };
    tabs.write().remove(removed_index);

    let remaining = tabs.read().clone();
    if remaining.is_empty() {
        active_tab_id.set(String::new());
        desktop.close();
        return;
    }

    if current_id == tab_id {
        let next_id = remaining
            .get(removed_index)
            .or_else(|| removed_index.checked_sub(1).and_then(|i| remaining.get(i)))
            .map(|tab| tab.id.clone())
            .unwrap_or_else(|| remaining[0].id.clone());
        active_tab_id.set(next_id);
    } else if remaining.iter().all(|tab| tab.id != current_id) {
        active_tab_id.set(remaining[0].id.clone());
    }
}

/// 打开书请求通道：主窗口与阅读器内引用跳转共用。
static READER_TX: OnceLock<Mutex<Option<UnboundedSender<OpenBookRequest>>>> = OnceLock::new();
/// 阅读窗口首启时取走的接收端（每开一次窗口重新放一个）。
static READER_RX: OnceLock<Mutex<Option<UnboundedReceiver<OpenBookRequest>>>> = OnceLock::new();

fn tx_slot() -> &'static Mutex<Option<UnboundedSender<OpenBookRequest>>> {
    READER_TX.get_or_init(|| Mutex::new(None))
}

fn rx_slot() -> &'static Mutex<Option<UnboundedReceiver<OpenBookRequest>>> {
    READER_RX.get_or_init(|| Mutex::new(None))
}

/// 打开一本书：阅读窗口已存在则切到/新建对应页签，否则新建窗口。
pub fn open(book_id: String) {
    open_with_project(book_id, None);
}

/// 从学习计划打开一本书（携带来源计划 id）。
pub fn open_with_project(book_id: String, project_id: Option<String>) {
    open_with_project_and_citation(book_id, project_id, None);
}

/// 从学习计划打开一本书，并携带一条待跳转引用。
pub fn open_with_project_and_citation(
    book_id: String,
    project_id: Option<String>,
    citation: Option<crate::model::BookCitation>,
) {
    if let Some(tx) = tx_slot().lock().ok().and_then(|g| g.clone())
        && tx
            .send(OpenBookRequest {
                book_id: book_id.clone(),
                project_id: project_id.clone(),
                citation: citation.clone(),
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
        citation,
    });
}

/// 阅读窗口根组件：订阅主窗口发来的打开书请求，以页签组织多本书。
#[component]
pub fn ReaderWindowRoot() -> Element {
    let tabs = use_signal(|| Vec::<ReaderTab>::new());
    let active_tab_id = use_signal(String::new);
    let error_signal = use_signal(ErrorSignal::new);
    use_context_provider(|| error_signal);
    let desktop = use_window();

    use_effect(move || {
        let rx = rx_slot().lock().ok().and_then(|mut g| g.take());
        spawn(async move {
            let Some(mut rx) = rx else {
                return;
            };
            let tabs = tabs;
            let active_tab_id = active_tab_id;
            while let Some(request) = rx.recv().await {
                open_request(tabs, active_tab_id, request);
            }
        });
    });

    // 阅读窗口标题统一跟随当前页签，避免隐藏页签在后台加载时覆盖标题。
    let title_desktop = desktop.clone();
    use_effect(move || {
        let active_id = active_tab_id.read().clone();
        let title = tabs
            .read()
            .iter()
            .find(|tab| tab.id == active_id)
            .map(|tab| tab.title.clone())
            .unwrap_or_else(|| "UeberNeon".to_string());
        let _ = title_desktop.set_title(&format!("UeberNeon — {title}"));
    });

    let err_modal = error_signal.read().modal.clone();
    let err_banner = error_signal.read().banner.clone();
    let tabs_now = tabs.read().clone();
    let active_id = active_tab_id.read().clone();
    let active_tab = tabs_now
        .iter()
        .find(|tab| tab.id == active_id)
        .or_else(|| tabs_now.first())
        .cloned();
    let displayed_active_id = active_tab
        .as_ref()
        .map(|tab| tab.id.clone())
        .unwrap_or_default();

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
            if tabs_now.is_empty() {
                div {
                    class: "reader-loading",
                    span { class: "reader-spinner" }
                    span { class: "reader-loading__label", "正在打开…" }
                }
            } else {
                div {
                    class: "reader-tabs-bar",
                    for tab in &tabs_now {
                        {
                            let tab_id = tab.id.clone();
                            let tab_title = tab.title.clone();
                            let title_attr = tab.title.clone();
                            let close_id = tab.id.clone();
                            let is_active = tab.id == active_id;
                            let tab_desktop = desktop.clone();
                            rsx! {
                                div {
                                    class: if is_active {
                                        "reader-tab is-active"
                                    } else {
                                        "reader-tab"
                                    },
                                    title: "{title_attr}",
                                    onclick: move |_| {
                                        activate_tab(tabs, active_tab_id, tab_id.clone());
                                    },
                                    span {
                                        class: "reader-tab-title",
                                        "{tab_title}"
                                    }
                                    button {
                                        class: "reader-tab-close",
                                        "aria-label": "关闭页签",
                                        onclick: move |evt: MouseEvent| {
                                            evt.stop_propagation();
                                            close_tab(
                                                tabs,
                                                active_tab_id,
                                                tab_desktop.clone(),
                                                close_id.clone(),
                                            );
                                        },
                                        "✕"
                                    }
                                }
                            }
                        }
                    }
                }
                div {
                    class: "reader-panels",
                    for tab in &tabs_now {
                        {
                            let close_id = tab.id.clone();
                            let is_active = tab.id == displayed_active_id;
                            let panel_tab = tab.clone();
                            let panel_desktop = desktop.clone();
                            rsx! {
                                div {
                                    key: "reader-panel-slot-{tab.id}",
                                    class: if is_active {
                                        "reader-panel-slot is-active"
                                    } else {
                                        "reader-panel-slot is-hidden"
                                    },
                                    ReaderPanel {
                                        key: "{panel_tab.id}-{panel_tab.revision}-{panel_tab.project_id:?}",
                                        book_id: panel_tab.book_id.clone(),
                                        project_id: panel_tab.project_id.clone(),
                                        initial_citation: panel_tab.citation.clone(),
                                        error_signal,
                                        manage_window_title: Some(false),
                                        on_back: move |_| {
                                            close_tab(
                                                tabs,
                                                active_tab_id,
                                                panel_desktop.clone(),
                                                close_id.clone(),
                                            );
                                        },
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
