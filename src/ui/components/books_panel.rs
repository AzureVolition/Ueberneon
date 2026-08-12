// ── 书库面板(主区域) ──
//
// 展示全局书库:书名称、路径、创建日期和被哪些学习计划引入;
// 支持从 PDF 导入新书,导入后后台提取知识库文本(pages/*.md);
// 点击书进入全屏阅读器。

use dioxus::prelude::*;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::ui::components::error::{ErrorInfo, ErrorSeverity, ErrorSignal, ErrorSource};

/// 组件卸载时置 false,让 OCR 进度轮询任务退出,避免跨导航泄漏。
struct OcrPollGuard(Arc<AtomicBool>);

impl Drop for OcrPollGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Relaxed);
    }
}

#[component]
pub fn BooksPanel(error_signal: Signal<ErrorSignal>, on_open_book: Callback<String>) -> Element {
    let mut books = use_signal(|| {
        crate::db::with_db(|conn| crate::books::list_with_projects(conn).unwrap_or_default())
    });
    let importing = use_signal(|| Option::<String>::None);
    let parsing = use_signal(HashSet::<String>::new);
    // (已完成 OCR 页数, 总页数, 是否正在后台 OCR)
    let ocr_progress = use_signal(|| HashMap::<String, (u32, u32, bool)>::new());
    // 书库右键菜单:None 或 (x, y, book_id)
    let mut book_context_menu = use_signal(|| Option::<(f64, f64, String)>::None);

    // 后台 OCR 进度轮询:任务进行中时每秒刷新书库状态;组件卸载即退出。
    let books_signal = books;
    use_effect(move || {
        let mut progress = ocr_progress;
        let alive = Arc::new(AtomicBool::new(true));
        let _guard = OcrPollGuard(alive.clone());
        spawn(async move {
            loop {
                if !alive.load(Ordering::Relaxed) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1000)).await;
                let mut snap = HashMap::new();
                for item in books_signal.read().iter() {
                    let id = item.book.id.clone();
                    let dir = Path::new(&item.book.path);
                    let running = crate::page_ocr::manager().is_running(&id);
                    let done = crate::page_ocr::load_progress(dir)
                        .map(|p| p.done.len() as u32)
                        .unwrap_or(0);
                    let total = crate::pdf::read_parse_marker(dir)
                        .map(|m| m.page_count)
                        .unwrap_or(0);
                    if running || done > 0 {
                        snap.insert(id, (done, total, running));
                    }
                }
                progress.set(snap);
            }
        });
    });

    let refresh = move |_| {
        if let Err(e) = crate::db::with_db(|conn| crate::books::sync_from_disk(conn)) {
            error_signal.write().push(ErrorInfo::new(
                "books-refresh-failed",
                "refresh books failed",
                format!("{e:#}"),
                ErrorSeverity::Warning,
                ErrorSource::General,
            ));
        }
        books.set(crate::db::with_db(|conn| {
            crate::books::list_with_projects(conn).unwrap_or_default()
        }));
    };

    // 选择多个 PDF:先逐本登记 + 复制原件,再逐本后台提取知识库文本。
    let on_import = move |_| {
        let mut importing = importing;
        let mut books = books;
        let mut parsing = parsing;
        let mut err = error_signal;
        spawn(async move {
            let files = rfd::AsyncFileDialog::new()
                .add_filter("PDF", &["pdf"])
                .pick_files()
                .await;
            let Some(files) = files else {
                return;
            };
            let mut imported = Vec::new();
            for file in files {
                let path = file.path().to_path_buf();
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("…")
                    .to_string();
                importing.set(Some(name.clone()));
                let result = tokio::task::spawn_blocking(move || {
                    crate::db::with_db(|conn| crate::books::import_pdf_file(conn, &path))
                })
                .await;
                let book_id = match result {
                    Ok(Ok(id)) => Some(id),
                    Ok(Err(e)) => {
                        err.write().push(ErrorInfo::new(
                            "book-import-failed",
                            "import book failed",
                            format!("{e:#}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                        None
                    }
                    Err(e) => {
                        err.write().push(ErrorInfo::new(
                            "book-import-failed",
                            "import book failed",
                            format!("{e}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                        None
                    }
                };
                if let Some(book_id) = book_id {
                    imported.push(book_id);
                }
            }
            importing.set(None);

            // 逐本后台解析(pages/*.md + parsed.json)
            for book_id in imported {
                parsing.write().insert(book_id.clone());
                let parse_id = book_id.clone();
                let parse_result =
                    tokio::task::spawn_blocking(move || crate::pdf::parse_book(&parse_id)).await;
                parsing.write().remove(&book_id);
                match parse_result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        err.write().push(ErrorInfo::new(
                            "book-parse-failed",
                            "parse book failed",
                            format!("{e:#}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                    Err(e) => {
                        err.write().push(ErrorInfo::new(
                            "book-parse-failed",
                            "parse book failed",
                            format!("{e}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                }
                // 自动整本 OCR(仅无文本页;未配置模型时静默跳过)。
                crate::page_ocr::manager().ensure_started(&book_id);
            }
            books.set(crate::db::with_db(|conn| {
                crate::books::list_with_projects(conn).unwrap_or_default()
            }));
        });
    };

    let on_delete_book = Callback::new(move |book_id: String| {
        let mut books = books;
        let mut ocr_progress = ocr_progress;
        let mut err = error_signal;
        spawn(async move {
            crate::page_ocr::manager().cancel(&book_id);
            let delete_id = book_id.clone();
            let result = tokio::task::spawn_blocking(move || {
                crate::db::with_db_result(|conn| crate::books::delete(conn, &delete_id))
            })
            .await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    err.write().push(ErrorInfo::new(
                        "book-delete-failed",
                        "delete book failed",
                        format!("{e}"),
                        ErrorSeverity::Warning,
                        ErrorSource::General,
                    ));
                }
                Err(e) => {
                    err.write().push(ErrorInfo::new(
                        "book-delete-failed",
                        "delete book failed",
                        format!("{e}"),
                        ErrorSeverity::Warning,
                        ErrorSource::General,
                    ));
                }
            }
            books.set(crate::db::with_db(|conn| {
                crate::books::list_with_projects(conn).unwrap_or_default()
            }));
            ocr_progress.write().remove(&book_id);
        });
    });

    let items = books.read().clone();
    let importing_now = importing.read().clone();
    let parsing_now = parsing.read().clone();

    rsx! {
        div {
            class: "books-panel",
            div {
                class: "books-panel-header",
                div {
                    class: "books-panel-heading",
                    span { class: "books-panel-title", "书库" }
                    span { class: "books-panel-subtitle", "~/.ueberneon/books" }
                }
                button {
                    class: "btn btn-send",
                    disabled: importing_now.is_some(),
                    onclick: on_import,
                    if let Some(ref name) = importing_now {
                        "导入中… {name}"
                    } else {
                        "导入书"
                    }
                }
                button {
                    class: "btn btn-cancel",
                    onclick: refresh,
                    "refresh"
                }
            }
            if items.is_empty() {
                div {
                    class: "books-panel-empty",
                    "书库为空,点击“导入书”选择 PDF 添加书籍。"
                }
            } else {
                div {
                    class: "books-panel-list",
                    for item in items.iter() {
                        {
                            let name = item.book.name.clone();
                            let book_id = item.book.id.clone();
                            let path = item.book.path.clone();
                            let created = item.book.created_at.chars().take(10).collect::<String>();
                            let refs = item
                                .projects
                                .iter()
                                .map(|p| p.name.clone())
                                .collect::<Vec<_>>()
                                .join(", ");
                            let status = if parsing_now.contains(&item.book.id) {
                                "解析中".to_string()
                            } else if let Some((done, total, running)) =
                                ocr_progress.read().get(&item.book.id).copied()
                            {
                                if running {
                                    format!("OCR {done}/{total}")
                                } else if total > 0 {
                                    format!("已解析 {total} 页(含 OCR {done})")
                                } else {
                                    format!("已 OCR {done} 页")
                                }
                            } else if let Some(marker) =
                                crate::pdf::read_parse_marker(Path::new(&item.book.path))
                            {
                                format!("已解析 {} 页", marker.page_count)
                            } else {
                                "未解析".to_string()
                            };
                            let badge_class = if parsing_now.contains(&item.book.id) {
                                "books-panel-item-badge parsing"
                            } else if let Some((_, _, running)) =
                                ocr_progress.read().get(&item.book.id).copied()
                            {
                                if running {
                                    "books-panel-item-badge parsing"
                                } else {
                                    "books-panel-item-badge parsed"
                                }
                            } else if crate::pdf::read_parse_marker(Path::new(&item.book.path))
                                .is_some()
                            {
                                "books-panel-item-badge parsed"
                            } else {
                                "books-panel-item-badge"
                            };
                            rsx! {
                                div {
                                    class: "books-panel-item",
                                    onclick: {
                                        let id = book_id.clone();
                                        move |_| {
                                            book_context_menu.set(None);
                                            on_open_book.call(id.clone())
                                        }
                                    },
                                    oncontextmenu: {
                                        let id = book_id.clone();
                                        move |evt| {
                                            evt.prevent_default();
                                            let coords = evt.client_coordinates();
                                            book_context_menu.set(Some((
                                                coords.x,
                                                coords.y,
                                                id.clone(),
                                            )));
                                        }
                                    },
                                    div {
                                        class: "books-panel-item-main",
                                        div {
                                            class: "books-panel-item-title-row",
                                            span { class: "books-panel-item-name", "{name}" }
                                            span { class: badge_class, "{status}" }
                                        }
                                        span { class: "books-panel-item-path", "{path}" }
                                    }
                                    div {
                                        class: "books-panel-item-meta",
                                        if refs.is_empty() {
                                            span { class: "books-panel-item-refs empty", "未引入" }
                                        } else {
                                            span { class: "books-panel-item-refs", "引入于: {refs}" }
                                        }
                                        span { class: "books-panel-item-created", "created {created}" }
                                        if let Some((_, _, running)) =
                                            ocr_progress.read().get(&item.book.id).copied()
                                        {
                                            if running {
                                                button {
                                                    class: "btn btn-cancel books-panel-item-ocr-stop",
                                                    onclick: {
                                                        let id = book_id.clone();
                                                        move |evt: MouseEvent| {
                                                            evt.stop_propagation();
                                                            crate::page_ocr::manager().cancel(&id);
                                                        }
                                                    },
                                                    "停止 OCR"
                                                }
                                            } else {
                                                button {
                                                    class: "btn btn-cancel books-panel-item-ocr-stop",
                                                    onclick: {
                                                        let id = book_id.clone();
                                                        move |evt: MouseEvent| {
                                                            evt.stop_propagation();
                                                            crate::page_ocr::manager().start(&id);
                                                        }
                                                    },
                                                    "重新 OCR"
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
            {
                let guard = book_context_menu.read();
                let val = guard.as_ref().map(|(x, y, book_id)| {
                    let bid = book_id.clone();
                    let pos_x = *x;
                    let pos_y = *y;
                    rsx! {
                        div {
                            class: "context-menu-overlay",
                            onclick: move |_| { book_context_menu.set(None); },
                            div {
                                class: "context-menu",
                                style: "left: {pos_x}px; top: {pos_y}px;",
                                div {
                                    class: "context-menu-item danger",
                                    onclick: move |_| {
                                        book_context_menu.set(None);
                                        on_delete_book.call(bid.clone());
                                    },
                                    "删除"
                                }
                            }
                        }
                    }
                });
                val
            }
        }
    }
}
