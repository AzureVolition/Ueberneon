// ── 书库面板(主区域) ──
//
// 展示全局书库:书名称、路径、创建日期和被哪些学习计划引入;
// 支持从 PDF 导入新书(本轮只复制原件并登记,解析/阅读器下一阶段)。

use dioxus::prelude::*;
use std::path::Path;

use crate::ui::components::error::{ErrorInfo, ErrorSeverity, ErrorSignal, ErrorSource};

#[component]
pub fn BooksPanel(error_signal: Signal<ErrorSignal>) -> Element {
    let mut books = use_signal(|| {
        crate::db::with_db(|conn| crate::books::list_with_projects(conn).unwrap_or_default())
    });
    let importing = use_signal(|| Option::<String>::None);

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

    // 选择多个 PDF 并逐本导入(复制 + 入库,解析下一阶段做)
    let on_import = move |_| {
        let mut importing = importing;
        let mut books = books;
        let mut err = error_signal;
        spawn(async move {
            let files = rfd::AsyncFileDialog::new()
                .add_filter("PDF", &["pdf"])
                .pick_files()
                .await;
            let Some(files) = files else {
                return;
            };
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
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        err.write().push(ErrorInfo::new(
                            "book-import-failed",
                            "import book failed",
                            format!("{e:#}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                    Err(e) => {
                        err.write().push(ErrorInfo::new(
                            "book-import-failed",
                            "import book failed",
                            format!("{e}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                }
            }
            importing.set(None);
            books.set(crate::db::with_db(|conn| {
                crate::books::list_with_projects(conn).unwrap_or_default()
            }));
        });
    };

    let items = books.read().clone();
    let importing_now = importing.read().clone();

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
                            let path = item.book.path.clone();
                            let created = item.book.created_at.chars().take(10).collect::<String>();
                            let parsed = Path::new(&item.book.path).join("pages").is_dir();
                            let refs = item
                                .projects
                                .iter()
                                .map(|p| p.name.clone())
                                .collect::<Vec<_>>()
                                .join(", ");
                            rsx! {
                                div {
                                    class: "books-panel-item",
                                    onclick: move |_| {
                                        error_signal.write().push(ErrorInfo::new(
                                            "book-reader-pending",
                                            "阅读器尚未开放",
                                            "PDF 解析与阅读器将在下一阶段实现",
                                            ErrorSeverity::Info,
                                            ErrorSource::General,
                                        ));
                                    },
                                    div {
                                        class: "books-panel-item-main",
                                        div {
                                            class: "books-panel-item-title-row",
                                            span { class: "books-panel-item-name", "{name}" }
                                            if !parsed {
                                                span { class: "books-panel-item-badge", "未解析" }
                                            }
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
