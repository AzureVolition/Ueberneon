// ── 学习计划内容面板（单计划视图）──
//
// 在计划内从「对话」切换到「内容」时展示：目前包含书籍区块，
// 支持从书库引入、导入 PDF、打开书、移除书。

use dioxus::prelude::*;
use std::collections::HashSet;
use std::path::Path;

use crate::ui::components::error::{ErrorInfo, ErrorSeverity, ErrorSignal, ErrorSource};

#[component]
pub fn PlanContentsPanel(
    project_id: String,
    error_signal: Signal<ErrorSignal>,
) -> Element {
    let project_id_toggle = project_id.clone();
    let project_id_import = project_id.clone();
    let books_list = use_signal(|| {
        crate::db::with_db(|conn| crate::books::list(conn).unwrap_or_default())
    });
    let mut selected_books = use_signal(HashSet::<String>::new);
    let mut books_dialog = use_signal(|| false);
    let importing = use_signal(|| Option::<String>::None);

    let toggle_book = Callback::new(move |(book_id, on): (String, bool)| {
        let mut selected = selected_books;
        let mut err = error_signal;
        let result = crate::db::with_db(|conn| {
            if on {
                crate::books::add_to_project(conn, &project_id_toggle, &book_id)
            } else {
                crate::books::remove_from_project(conn, &project_id_toggle, &book_id)
            }
        });
        match result {
            Ok(()) => {
                if on {
                    selected.write().insert(book_id);
                } else {
                    selected.write().remove(&book_id);
                }
            }
            Err(e) => {
                err.write().push(ErrorInfo::new(
                    "book-link-failed",
                    "update books failed",
                    e,
                    ErrorSeverity::Warning,
                    ErrorSource::General,
                ));
            }
        }
    });

    let refresh_books = {
        let mut list = books_list;
        move |_| {
            let _ = crate::db::with_db(crate::books::sync_from_disk);
            list.set(crate::db::with_db(|conn| {
                crate::books::list(conn).unwrap_or_default()
            }));
        }
    };

    let on_import_pdf = Callback::new(move |_| {
        let mut importing = importing;
        let mut books_list = books_list;
        let mut selected = selected_books;
        let mut err = error_signal;
        let pid = project_id_import.clone();
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
                if let Some(id) = book_id {
                    let _ = crate::db::with_db(|conn| {
                        crate::books::add_to_project(conn, &pid, &id)
                    });
                    selected.write().insert(id.clone());
                    imported.push(id);
                }
            }
            importing.set(None);
            for book_id in imported {
                let parse_id = book_id.clone();
                let _ = tokio::task::spawn_blocking(move || crate::pdf::parse_book(&parse_id))
                    .await;
                crate::page_ocr::manager().ensure_started(&book_id);
            }
            books_list.set(crate::db::with_db(|conn| {
                crate::books::list(conn).unwrap_or_default()
            }));
        });
    });

    let plan_name = crate::db::with_db(|conn| {
        crate::db::metadata::project::get(conn, &project_id)
            .ok()
            .flatten()
            .map(|r| r.name)
            .unwrap_or_default()
    });
    let plan_books = crate::db::with_db(|conn| {
        crate::books::list_by_project(conn, &project_id).unwrap_or_default()
    });
    let books = books_list.read().clone();
    let selected = selected_books.read().clone();

    rsx! {
        div {
            class: "plan-contents",
            div {
                class: "plan-contents-header",
                div {
                    class: "plan-contents-heading",
                    span { class: "plan-contents-title", "计划内容" }
                    span { class: "plan-contents-subtitle", "{plan_name}" }
                }
                div {
                    class: "plan-contents-actions",
                    button {
                        class: "btn btn-cancel",
                        onclick: {
                            let pid = project_id.clone();
                            move |_| {
                            selected_books.set(
                                crate::db::with_db(|conn| {
                                    crate::books::project_book_ids(conn, &pid)
                                        .unwrap_or_default()
                                })
                                .into_iter()
                                .collect(),
                            );
                            books_dialog.set(true);
                            }
                        },
                        "从书库引入"
                    }
                    button {
                        class: "btn btn-send",
                        onclick: {
                            let on_import = on_import_pdf;
                            move |_| on_import.call(())
                        },
                        if importing.read().is_some() {
                            "导入中…"
                        } else {
                            "导入 PDF"
                        }
                    }
                }
            }

            div {
                class: "plan-card-books",
                div {
                    class: "plan-card-books-head",
                    span { class: "plan-card-books-label", "books" }
                }
                if plan_books.is_empty() {
                    div {
                        class: "plan-card-books-empty",
                        "未引入书籍 — 从书库选择或导入 PDF。"
                    }
                } else {
                    div {
                        class: "plan-card-books-list",
                        for book in plan_books.iter() {
                            {
                                let bid = book.id.clone();
                                let bname = book.name.clone();
                                let bpath = book.path.clone();
                                let status = crate::pdf::read_parse_marker(
                                    Path::new(&book.path),
                                )
                                .map(|m| format!("{} 页", m.page_count))
                                .unwrap_or_else(|| "未解析".to_string());
                                rsx! {
                                    div {
                                        class: "plan-card-book",
                                        onclick: {
                                            let pid = project_id.clone();
                                            let bid2 = bid.clone();
                                            move |_| {
                                                crate::ui::reader_window::open_with_project(
                                                    bid2.clone(),
                                                    Some(pid.clone()),
                                                )
                                            }
                                        },
                                        div {
                                            class: "plan-card-book-main",
                                            span { class: "plan-card-book-name", "{bname}" }
                                            span { class: "plan-card-book-path", "{bpath}" }
                                        }
                                        span { class: "plan-card-book-status", "{status}" }
                                        button {
                                            class: "btn btn-cancel plan-card-book-remove",
                                            onclick: {
                                                let bid2 = bid.clone();
                                                let pid = project_id.clone();
                                                move |evt: MouseEvent| {
                                                    evt.stop_propagation();
                                                    toggle_book.call((bid2.clone(), false));
                                                    let _ = crate::db::with_db(|conn| {
                                                        crate::books::remove_from_project(
                                                            conn,
                                                            &pid,
                                                            &bid2,
                                                        )
                                                    });
                                                }
                                            },
                                            "移除"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if *books_dialog.read() {
                {
                    rsx! {
                        div {
                            class: "books-dialog-overlay",
                            onclick: move |_| books_dialog.set(false),
                            div {
                                class: "books-dialog",
                                onclick: move |evt| evt.stop_propagation(),
                                div {
                                    class: "books-dialog-header",
                                    span {
                                        class: "books-dialog-title",
                                        "books · {plan_name}"
                                    }
                                    div {
                                        class: "books-dialog-actions",
                                        button {
                                            class: "btn btn-cancel",
                                            onclick: refresh_books,
                                            "refresh"
                                        }
                                        button {
                                            class: "btn btn-cancel",
                                            onclick: {
                                                let on_import = on_import_pdf;
                                                move |_| on_import.call(())
                                            },
                                            "导入 PDF"
                                        }
                                        button {
                                            class: "btn btn-cancel",
                                            onclick: move |_| books_dialog.set(false),
                                            "close"
                                        }
                                    }
                                }
                                if books.is_empty() {
                                    div {
                                        class: "books-dialog-empty",
                                        "书库为空 — 先点“导入 PDF”添加书籍。"
                                    }
                                } else {
                                    div {
                                        class: "books-dialog-list",
                                        for book in books.iter() {
                                            {
                                                let bid = book.id.clone();
                                                let bname = book.name.clone();
                                                let bpath = book.path.clone();
                                                let checked = selected.contains(&bid);
                                                rsx! {
                                                    div {
                                                        class: if checked {
                                                            "books-dialog-item checked"
                                                        } else {
                                                            "books-dialog-item"
                                                        },
                                                        onclick: {
                                                            let bid2 = bid.clone();
                                                            move |_| {
                                                                toggle_book.call((
                                                                    bid2.clone(),
                                                                    !checked,
                                                                ))
                                                            }
                                                        },
                                                        span {
                                                            class: "books-dialog-check",
                                                            if checked { "✓" } else { "" }
                                                        }
                                                        div {
                                                            class: "books-dialog-info",
                                                            span {
                                                                class: "books-dialog-name",
                                                                "{bname}"
                                                            }
                                                            span {
                                                                class: "books-dialog-path",
                                                                "{bpath}"
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
                }
            }
        }
    }
}
