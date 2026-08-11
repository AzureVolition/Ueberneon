// ── 书库面板(主区域) ──
//
// 展示全局书库:书名称、路径、创建日期和被哪些学习计划引入;
// 支持从 PDF 导入新书,导入后后台提取知识库文本(pages/*.md);
// 点击书进入全屏阅读器。

use dioxus::prelude::*;
use std::collections::HashSet;
use std::path::Path;

use crate::ui::components::error::{ErrorInfo, ErrorSeverity, ErrorSignal, ErrorSource};

#[component]
pub fn BooksPanel(error_signal: Signal<ErrorSignal>, on_open_book: Callback<String>) -> Element {
    let mut books = use_signal(|| {
        crate::db::with_db(|conn| crate::books::list_with_projects(conn).unwrap_or_default())
    });
    let importing = use_signal(|| Option::<String>::None);
    let parsing = use_signal(HashSet::<String>::new);

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
            }
            books.set(crate::db::with_db(|conn| {
                crate::books::list_with_projects(conn).unwrap_or_default()
            }));
        });
    };

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
                            } else if let Some(marker) =
                                crate::pdf::read_parse_marker(Path::new(&item.book.path))
                            {
                                format!("已解析 {} 页", marker.page_count)
                            } else {
                                "未解析".to_string()
                            };
                            let badge_class = if parsing_now.contains(&item.book.id) {
                                "books-panel-item-badge parsing"
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
                                        move |_| on_open_book.call(id.clone())
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
