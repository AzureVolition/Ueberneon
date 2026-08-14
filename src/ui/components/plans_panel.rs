// ── 学习计划首页（主区域）──
//
// 计划卡片 + 书籍区块：从书库勾选引入、直接导入 PDF、点击书打开阅读器
// （携带计划 id，供阅读器顶栏显示来源）。

use dioxus::prelude::*;
use std::collections::HashSet;
use std::path::Path;

use crate::model::Project;
use crate::ui::components::error::{ErrorInfo, ErrorSeverity, ErrorSignal, ErrorSource};

#[component]
pub fn PlansPanel(
    projects: Signal<Vec<Project>>,
    error_signal: Signal<ErrorSignal>,
    on_select_project: EventHandler<String>,
    on_new_project: EventHandler<String>,
    on_delete_project: EventHandler<String>,
) -> Element {
    // ── 书籍引入弹窗状态 ──
    let mut books_dialog = use_signal(|| Option::<String>::None);
    let books_list = use_signal(|| {
        crate::db::with_db(|conn| crate::books::list(conn).unwrap_or_default())
    });
    let mut selected_books = use_signal(HashSet::<String>::new);
    let importing = use_signal(|| Option::<String>::None);
    let mut new_project_form = use_signal(|| false);
    let mut new_project_name = use_signal(String::new);

    let toggle_book = Callback::new(move |(book_id, on): (String, bool)| {
        let dialog = books_dialog;
        let mut selected = selected_books;
        let mut err = error_signal;
        let Some(pid) = dialog.read().clone() else {
            return;
        };
        let result = crate::db::with_db(|conn| {
            if on {
                crate::books::add_to_project(conn, &pid, &book_id)
            } else {
                crate::books::remove_from_project(conn, &pid, &book_id)
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

    // 导入 PDF：先入全局书库，再自动引入当前计划，最后触发解析/OCR。
    let on_import_pdf = Callback::new(move |pid: String| {
        let mut importing = importing;
        let mut books_list = books_list;
        let mut selected = selected_books;
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
                if let Some(id) = book_id {
                    let pid2 = pid.clone();
                    let _ = crate::db::with_db(|conn| {
                        crate::books::add_to_project(conn, &pid2, &id)
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

    let dialog_pid = books_dialog.read().clone();
    let dialog_proj_name = dialog_pid
        .as_ref()
        .and_then(|pid| {
            projects
                .read()
                .iter()
                .find(|p| &p.id == pid)
                .map(|p| p.name.clone())
        })
        .unwrap_or_default();

    rsx! {
        div {
            class: "plans-panel",
            div {
                class: "plans-panel-header",
                div {
                    class: "plans-panel-heading",
                    span { class: "plans-panel-title", "学习计划" }
                    span { class: "plans-panel-subtitle", "每个计划可引入书籍并打开书旁对话" }
                }
                button {
                    class: "btn btn-send",
                    onclick: move |_| {
                        let mut show = new_project_form.write();
                        *show = !*show;
                        if *show {
                            new_project_name.set(String::new());
                        }
                    },
                    if *new_project_form.read() { "−" } else { "+ NEW" }
                }
            }

            if *new_project_form.read() {
                div {
                    class: "plans-panel-new",
                    input {
                        class: "plans-panel-new-input",
                        value: new_project_name,
                        placeholder: "plan name",
                        oninput: move |evt| new_project_name.set(evt.value()),
                    }
                    button {
                        class: "btn btn-cancel",
                        onclick: move |_| new_project_form.set(false),
                        "cancel"
                    }
                    button {
                        class: "btn btn-send",
                        onclick: move |_| {
                            let name = new_project_name.read().trim().to_string();
                            if !name.is_empty() {
                                on_new_project.call(name);
                                new_project_form.set(false);
                                new_project_name.set(String::new());
                            }
                        },
                        "save"
                    }
                }
            }

            if projects.read().iter().all(|p| p.id == crate::db::DEFAULT_PROJECT_ID) {
                div {
                    class: "plans-panel-empty",
                    "还没有学习计划，点击 + NEW 创建第一个。"
                }
            } else {
                div {
                    class: "plans-panel-list",
                    for proj in projects.read().iter().filter(|p| p.id != crate::db::DEFAULT_PROJECT_ID) {
                        {
                            let pid = proj.id.clone();
                            let pname = proj.name.clone();
                            let color = if proj.indicator_color.is_empty() {
                                "cyan".to_string()
                            } else {
                                proj.indicator_color.clone()
                            };
                            let activity = proj
                                .last_activity_at
                                .map(|t| crate::model::format_relative_time(&t))
                                .unwrap_or_else(|| "—".to_string());
                            let plan_books =
                                crate::db::with_db(|conn| {
                                    crate::books::list_by_project(conn, &pid).unwrap_or_default()
                                });
                            rsx! {
                                div {
                                    class: "plan-card",
                                    "data-color": "{color}",
                                    div {
                                        class: "plan-card-head",
                                        span { class: "plan-card-indicator" }
                                        div {
                                            class: "plan-card-title-block",
                                            span { class: "plan-card-name", "{pname}" }
                                            span { class: "plan-card-meta", "active {activity}" }
                                        }
                                        div {
                                            class: "plan-card-actions",
                                            button {
                                                class: "btn btn-cancel",
                                                onclick: {
                                                    let pid2 = pid.clone();
                                                    move |_| {
                                                        books_dialog.set(Some(pid2.clone()));
                                                        selected_books.set(
                                                            crate::db::with_db(|conn| {
                                                                crate::books::project_book_ids(
                                                                    conn, &pid2,
                                                                )
                                                                .unwrap_or_default()
                                                            })
                                                            .into_iter()
                                                            .collect(),
                                                        );
                                                    }
                                                },
                                                "从书库引入"
                                            }
                                            button {
                                                class: "btn btn-send",
                                                onclick: {
                                                    let pid2 = pid.clone();
                                                    move |_| on_select_project.call(pid2.clone())
                                                },
                                                "进入计划"
                                            }
                                            button {
                                                class: "btn btn-cancel danger",
                                                onclick: {
                                                    let pid2 = pid.clone();
                                                    move |_| on_delete_project.call(pid2.clone())
                                                },
                                                "删除"
                                            }
                                        }
                                    }
                                    div {
                                        class: "plan-card-books",
                                        div {
                                            class: "plan-card-books-head",
                                            span { class: "plan-card-books-label", "books" }
                                            button {
                                                class: "btn btn-cancel",
                                                onclick: {
                                                    let pid2 = pid.clone();
                                                    move |_| on_import_pdf.call(pid2.clone())
                                                },
                                                if importing.read().is_some() {
                                                    "导入中…"
                                                } else {
                                                    "导入 PDF"
                                                }
                                            }
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
                                                                    let pid2 = pid.clone();
                                                                    let bid2 = bid.clone();
                                                                    move |_| {
                                                                        crate::ui::reader_window::open_with_project(
                                                                            bid2.clone(),
                                                                            Some(pid2.clone()),
                                                                        )
                                                                    }
                                                                },
                                                                div {
                                                                    class: "plan-card-book-main",
                                                                    span {
                                                                        class: "plan-card-book-name",
                                                                        "{bname}"
                                                                    }
                                                                    span {
                                                                        class: "plan-card-book-path",
                                                                        "{bpath}"
                                                                    }
                                                                }
                                                                span { class: "plan-card-book-status", "{status}" }
                                                                button {
                                                                    class: "btn btn-cancel plan-card-book-remove",
                                                                    onclick: {
                                                                        let pid2 = pid.clone();
                                                                        let bid2 = bid.clone();
                                                                        move |evt: MouseEvent| {
                                                                            evt.stop_propagation();
                                                                            toggle_book.call((
                                                                                bid2.clone(),
                                                                                false,
                                                                            ));
                                                                            let _ = crate::db::with_db(
                                                                                |conn| {
                                                                                    crate::books::remove_from_project(
                                                                                        conn,
                                                                                        &pid2,
                                                                                        &bid2,
                                                                                    )
                                                                                },
                                                                            );
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
                                }
                            }
                        }
                    }
                }
            }

            if let Some(pid) = dialog_pid {
                {
                let books = books_list.read().clone();
                let selected = selected_books.read().clone();
                rsx! {
                    div {
                        class: "books-dialog-overlay",
                        onclick: move |_| books_dialog.set(None),
                        div {
                            class: "books-dialog",
                            onclick: move |evt| evt.stop_propagation(),
                            div {
                                class: "books-dialog-header",
                                span { class: "books-dialog-title", "books · {dialog_proj_name}" }
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
                                            let pid2 = pid.clone();
                                            move |_| on_import_pdf.call(pid2.clone())
                                        },
                                        "导入 PDF"
                                    }
                                    button {
                                        class: "btn btn-cancel",
                                        onclick: move |_| books_dialog.set(None),
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
                                                        span { class: "books-dialog-name", "{bname}" }
                                                        span { class: "books-dialog-path", "{bpath}" }
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
