// ── Skills 管理面板 ──
//
// 磁盘即真相：列表来自 crate::skills::registry(project)，
// DB 只存 enabled/disabled 状态与用量；安装/卸载直接操作磁盘目录。

use std::path::{Path, PathBuf};

use dioxus::prelude::*;

use crate::db::metadata::skill as skill_state;
use crate::skills::{self, SkillEntry};

fn load_registry(project: &Path) -> Vec<SkillEntry> {
    skills::registry(project)
}

fn status_label(status: &str) -> String {
    match status {
        "enabled" => "enabled".into(),
        "disabled" => "disabled".into(),
        _ => status.to_string(),
    }
}

fn last_run_label(last: &Option<String>) -> String {
    match last {
        Some(ts) => chrono::DateTime::parse_from_rfc3339(ts)
            .map(|d| d.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|_| ts.clone()),
        None => "—".into(),
    }
}

#[component]
pub fn SkillsPanel(project_path: String) -> Element {
    let project = use_signal(move || PathBuf::from(project_path));
    let mut entries = use_signal(move || load_registry(&project()));
    let mut status_filter = use_signal(|| "all".to_string());
    let mut cat_filter = use_signal(|| "all".to_string());
    let mut search = use_signal(String::new);
    let mut selected = use_signal(|| Option::<String>::None);
    let mut toggling = use_signal(|| Option::<String>::None);
    let mut confirming_uninstall = use_signal(|| Option::<String>::None);
    let mut uninstalling = use_signal(|| Option::<String>::None);
    let mut show_config = use_signal(|| false);

    // install dialog
    let mut show_install = use_signal(|| false);
    let mut install_input = use_signal(String::new);
    let mut install_error = use_signal(|| Option::<String>::None);
    let mut installing = use_signal(|| false);

    let categories = use_memo(move || {
        let mut cats: Vec<String> = entries
            .read()
            .iter()
            .filter_map(|e| e.category.clone())
            .collect();
        cats.sort();
        cats.dedup();
        cats
    });

    let filtered = use_memo(move || {
        let all = entries.read();
        let st = status_filter.read().clone();
        let cat = cat_filter.read().clone();
        let q = search.read().trim().to_lowercase();
        all.iter()
            .filter(|e| {
                let status_ok = st == "all" || e.status == st;
                let cat_ok = cat == "all" || e.category.as_deref() == Some(cat.as_str());
                let query_ok = q.is_empty()
                    || e.name.to_lowercase().contains(&q)
                    || e.description.to_lowercase().contains(&q)
                    || e.category
                        .as_deref()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&q);
                status_ok && cat_ok && query_ok
            })
            .cloned()
            .collect::<Vec<_>>()
    });

    let (count_all, count_enabled, count_disabled) = {
        let all = entries.read();
        (
            all.len(),
            all.iter().filter(|e| e.status == "enabled").count(),
            all.iter().filter(|e| e.status == "disabled").count(),
        )
    };

    let selected_entry = {
        let list = filtered.read();
        let sel = selected.read();
        list.iter()
            .find(|e| Some(&e.name) == sel.as_ref())
            .or_else(|| list.first())
            .cloned()
    };

    let mut refresh = move || {
        entries.set(load_registry(&project()));
    };

    let mut do_toggle = move |name: String, status: String| {
        toggling.set(Some(name.clone()));
        let next = if status == "enabled" {
            "disabled"
        } else {
            "enabled"
        }
        .to_string();
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(280)).await;
            let _ = crate::db::with_db_result(|conn| skill_state::set_status(conn, &name, &next));
            toggling.set(None);
            refresh();
        });
    };

    let mut do_uninstall = move |name: String| {
        uninstalling.set(Some(name.clone()));
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(350)).await;
            let _ = skills::uninstall(&project(), &name);
            uninstalling.set(None);
            confirming_uninstall.set(None);
            selected.set(None);
            refresh();
        });
    };

    let mut close_install = move || {
        show_install.set(false);
        install_error.set(None);
        install_input.set(String::new());
    };

    let mut do_install = move || {
        let source = install_input.read().trim().to_string();
        if source.is_empty() {
            install_error.set(Some(
                "the source is empty. paste a git url or local path.".into(),
            ));
            return;
        }
        installing.set(true);
        install_error.set(None);
        spawn(async move {
            let result = tokio::task::spawn_blocking(move || skills::install(&source))
                .await
                .unwrap_or_else(|e| Err(format!("install task failed: {e}")));
            installing.set(false);
            show_install.set(false);
            install_input.set(String::new());
            install_error.set(None);
            match result {
                Ok(name) => {
                    refresh();
                    selected.set(Some(name));
                }
                Err(e) => {
                    show_install.set(true);
                    install_error.set(Some(e));
                }
            }
        });
    };

    let status_chips = [
        ("all", "all", count_all),
        ("enabled", "enabled", count_enabled),
        ("disabled", "disabled", count_disabled),
    ];

    let cats = categories.read().clone();
    let registry_empty = entries.read().is_empty();

    rsx! {
        div { class: "skills-panel",

            // ── header ──
            div { class: "skills-head",
                div { class: "skills-head__text",
                    span { class: "skills-mono", "ueberneon / capabilities" }
                    h2 { class: "skills-head__title",
                        "skills that "
                        em { "wire" }
                        " into the loop."
                    }
                    p { class: "skills-head__lede", "browse, enable, and install reusable capabilities for the agent. changes apply to the next task." }
                }
                div { class: "skills-head__actions",
                    div { class: "skills-searchpill",
                        span { class: "skills-searchpill__ico", aria_hidden: "true" }
                        input {
                            class: "skills-searchpill__input",
                            placeholder: "search skills",
                            value: "{search}",
                            oninput: move |e| search.set(e.value()),
                        }
                    }
                    button {
                        class: "btn skills-btn-accent",
                        onclick: move |_| show_install.set(true),
                        "install skill"
                    }
                }
            }

            // ── toolbar ──
            div { class: "skills-toolbar",
                div { class: "skills-chip-row", role: "group",
                    for (key, label, count) in status_chips {
                        button {
                            class: if status_filter() == key { "skills-chip is-active" } else { "skills-chip" },
                            onclick: move |_| status_filter.set(key.to_string()),
                            span { "{label}" }
                            span { class: "skills-chip__count", "{count}" }
                        }
                    }
                }
                div { class: "skills-chip-row skills-chip-row--cat", role: "group",
                    button {
                        class: if cat_filter() == "all" { "skills-chip skills-chip--quiet is-active" } else { "skills-chip skills-chip--quiet" },
                        onclick: move |_| cat_filter.set("all".to_string()),
                        "all categories"
                    }
                    for cat in &cats {
                        {
                            let cat = cat.clone();
                            let active = cat_filter() == cat;
                            rsx! {
                                button {
                                    class: if active { "skills-chip skills-chip--quiet is-active" } else { "skills-chip skills-chip--quiet" },
                                    onclick: move |_| cat_filter.set(cat.clone()),
                                    "{cat}"
                                }
                            }
                        }
                    }
                }
            }

            // ── workspace ──
            div { class: "skills-workspace",
                div { class: "skills-index",
                    div { class: "skills-index__head",
                        span { class: "settings-field-label", "installed · {count_all}" }
                        if status_filter() != "all" || cat_filter() != "all" || !search.read().is_empty() {
                            button {
                                class: "skills-clear",
                                onclick: move |_| {
                                    status_filter.set("all".to_string());
                                    cat_filter.set("all".to_string());
                                    search.set(String::new());
                                },
                                "clear filters"
                            }
                        }
                    }

                    if filtered.read().is_empty() {
                        div { class: "skills-empty",
                            if registry_empty {
                                p { class: "skills-empty__title", "no skills yet." }
                                p { class: "skills-empty__body", "install a skill to get started." }
                                button {
                                    class: "btn skills-btn-accent",
                                    onclick: move |_| show_install.set(true),
                                    "install skill"
                                }
                            } else {
                                p { class: "skills-empty__title", "no skills match." }
                                p { class: "skills-empty__body", "try another filter, or widen the search." }
                            }
                        }
                    } else {
                        div { class: "skills-list",
                            for e in filtered.read().iter() {
                                {
                                    let name = e.name.clone();
                                    let status = e.status.clone();
                                    let version = e.version.clone();
                                    let cat = e.category.clone();
                                    let is_selected = selected_entry.as_ref().map(|x| x.name == name).unwrap_or(false);
                                    let busy = toggling() == Some(name.clone());
                                    let select_name = name.clone();
                                    let toggle_name = name.clone();
                                    let toggle_status = status.clone();
                                    rsx! {
                                        div {
                                            class: if is_selected { "skills-row is-selected" } else { "skills-row" },
                                            onclick: move |_| selected.set(Some(select_name.clone())),
                                            div { class: "skills-row__select",
                                                span { class: "skills-status skills-status--{status}" }
                                                div { class: "skills-row__body",
                                                    span { class: "skills-name", "{name}" }
                                                    span { class: "skills-meta",
                                                        if let Some(ref c) = cat {
                                                            span { "{c}" }
                                                            span { "·" }
                                                        }
                                                        span { "{status_label(&status)}" }
                                                    }
                                                }
                                                span { class: "skills-version", "{version}" }
                                                span { class: "skills-chevron", "›" }
                                            }
                                            button {
                                                class: "skills-toggle",
                                                disabled: busy,
                                                aria_checked: status == "enabled",
                                                onclick: move |e| {
                                                    e.stop_propagation();
                                                    do_toggle(toggle_name.clone(), toggle_status.clone());
                                                },
                                                span { class: "skills-toggle__track",
                                                    span { class: "skills-toggle__thumb" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── detail ──
                div { class: "skills-detail",
                    if let Some(ref sk) = selected_entry {
                        {
                            let d_name = sk.name.clone();
                            let d_status = sk.status.clone();
                            let d_version = sk.version.clone();
                            let d_cat = sk.category.clone();
                            let d_desc = sk.description.clone();
                            let d_root = sk.root.display().to_string();
                            let d_usage = sk.usage_count;
                            let d_last = sk.last_run_at.clone();
                            let busy_toggle = toggling() == Some(d_name.clone());
                            let busy_uninstall = uninstalling() == Some(d_name.clone());
                            let confirming = confirming_uninstall() == Some(d_name.clone());
                            let toggle_name = d_name.clone();
                            let toggle_status = d_status.clone();
                            let uninstall_confirm_name = d_name.clone();
                            rsx! {
                                div { class: "skills-detail__head",
                                    span { class: "settings-field-label", "selected skill" }
                                    h3 { class: "skills-detail__title", "{d_name}" }
                                    p { class: "skills-detail__desc", "{d_desc}" }
                                }
                                dl { class: "skills-spec",
                                    if let Some(ref c) = d_cat {
                                        div { class: "skills-spec__row", dt { "category" } dd { "{c}" } }
                                    }
                                    div { class: "skills-spec__row", dt { "version" } dd { "{d_version}" } }
                                    div { class: "skills-spec__row", dt { "path" } dd { "{d_root}" } }
                                    div { class: "skills-spec__row", dt { "last run" } dd { "{last_run_label(&d_last)}" } }
                                    div { class: "skills-spec__row", dt { "usage" } dd {
                                        if d_usage > 0 { "{d_usage} tasks" } else { "—" }
                                    } }
                                }
                                div { class: "skills-usage",
                                    span { class: "settings-field-label", "usage" }
                                    pre { class: "skills-code",
                                        "request: {d_name}"
                                        span { class: "skills-caret", "▮" }
                                    }
                                }
                                div { class: "skills-actions",
                                    div { class: "skills-action-toggle",
                                        span { "{status_label(&d_status)}" }
                                        button {
                                            class: "skills-toggle",
                                            disabled: busy_toggle,
                                            aria_checked: d_status == "enabled",
                                            onclick: move |_| do_toggle(toggle_name.clone(), toggle_status.clone()),
                                            span { class: "skills-toggle__track",
                                                span { class: "skills-toggle__thumb" }
                                            }
                                        }
                                    }
                                    button {
                                        class: "btn skills-btn-ghost",
                                        onclick: move |_| show_config.set(!show_config()),
                                        if show_config() { "hide config" } else { "view config" }
                                    }
                                    button {
                                        class: "btn skills-btn-danger",
                                        disabled: busy_uninstall,
                                        onclick: move |_| {
                                            if confirming_uninstall() == Some(uninstall_confirm_name.clone()) {
                                                do_uninstall(uninstall_confirm_name.clone());
                                            } else {
                                                confirming_uninstall.set(Some(uninstall_confirm_name.clone()));
                                            }
                                        },
                                        if busy_uninstall { "uninstalling…" } else if confirming { "confirm uninstall" } else { "uninstall" }
                                    }
                                }
                                if show_config() {
                                    pre { class: "skills-config",
                                        "name: {d_name}\nversion: {d_version}\nstatus: {d_status}\npath: {d_root}"
                                    }
                                }
                            }
                        }
                    } else {
                        div { class: "skills-empty",
                            p { class: "skills-empty__title", "no skill selected." }
                        }
                    }
                }
            }

            // ── status strip ──
            div { class: "skills-status-strip",
                span { class: "settings-field-label", "active · {count_enabled}" }
                div { class: "skills-meter", aria_hidden: "true",
                    for i in 0..48 {
                        {
                            let h = 5.0 + 14.0 * (i as f64 * 0.42).sin().abs() + if i % 7 == 0 { 3.0 } else { 0.0 };
                            let o = 0.35 + 0.4 * (i as f64 * 0.31).sin().abs();
                            rsx! {
                                span { style: "height:{h:.1}px;opacity:{o:.2}" }
                            }
                        }
                    }
                }
                span { class: "settings-field-label", "disabled · {count_disabled}" }
            }
        }

        // ── install modal ──
        if show_install() {
            div {
                class: "settings-modal-backdrop",
                onclick: move |_| close_install(),
                div {
                    class: "settings-modal-panel skills-install-modal",
                    onclick: move |e| e.stop_propagation(),
                    div { class: "settings-modal-header",
                        span { class: "settings-modal-title", "install skill" }
                        button {
                            class: "settings-modal-close",
                            onclick: move |_| close_install(),
                            "✕"
                        }
                    }
                    div { class: "settings-modal-body",
                        p { class: "skills-install-title", "install from a git repo or local path." }
                        label { class: "settings-field-label", for: "skills-install-input", "source" }
                        input {
                            id: "skills-install-input",
                            class: if install_error().is_some() { "settings-input skills-install-input is-error" } else { "settings-input skills-install-input" },
                            placeholder: "github.com/user/skill-repo  or  /path/to/skill",
                            value: "{install_input}",
                            disabled: installing(),
                            oninput: move |e| {
                                install_input.set(e.value());
                                install_error.set(None);
                            },
                        }
                        if let Some(ref err) = install_error() {
                            p { class: "skills-error", "{err}" }
                        }
                        div { class: "skills-install-actions",
                            button {
                                class: "btn skills-btn-ghost",
                                onclick: move |_| close_install(),
                                "cancel"
                            }
                            button {
                                class: "btn skills-btn-accent",
                                disabled: installing(),
                                onclick: move |_| do_install(),
                                if installing() { "installing…" } else { "install" }
                            }
                        }
                    }
                }
            }
        }
    }
}
