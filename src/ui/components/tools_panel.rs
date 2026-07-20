// ── 工具配置面板 ──
//
// 两个子页签：
//   工具   — 分页查看所有工具，按组筛选 / 按名称搜索
//   工具组 — 管理工具组及其成员

use dioxus::prelude::*;

use crate::db::metadata::tool::*;
use crate::ui::components::dropdown::{Dropdown, DropdownOption};

#[derive(Clone, PartialEq)]
enum ToolsSubTab {
    Tools,
    Groups,
}

#[derive(Clone)]
struct GroupItem {
    id: String,
    name: String,
    description: String,
    tool_count: i64,
    is_deleting: bool,
}

#[derive(Clone)]
struct EditToolItem {
    id: String,
    name: String,
    description: String,
    is_in_group: bool,
}

#[component]
pub fn ToolsPanel() -> Element {
    let mut subtab = use_signal(|| ToolsSubTab::Tools);
    let mut page = use_signal(|| 0i64);
    let page_size = 10i64;
    let mut filter_group = use_signal(|| Option::<String>::None);
    let mut search_text = use_signal(String::new);
    let mut groups_cache: Signal<Vec<ToolGroupRow>> = use_signal(|| {
        crate::db::with_db(|conn| list_groups(conn).unwrap_or_default())
    });

    let tools_data = use_memo(move || {
        let gid = filter_group.read().clone();
        let s = search_text.read();
        let search = if s.is_empty() { None } else { Some(s.as_str()) };
        crate::db::with_db(|conn| {
            let list = list_tools_paginated(conn, gid.as_deref(), search, page_size, page() * page_size).unwrap_or_default();
            let total = count_tools(conn, gid.as_deref(), search).unwrap_or(0);
            (list, total)
        })
    });

    let mut groups: Signal<Vec<ToolGroupRow>> = use_signal(|| {
        crate::db::with_db(|conn| list_groups(conn).unwrap_or_default())
    });
    let mut group_tool_count: Signal<std::collections::HashMap<String, i64>> = use_signal(|| {
        crate::db::with_db(|conn| {
            let grps = list_groups(conn).unwrap_or_default();
            let mut map = std::collections::HashMap::new();
            for g in &grps {
                if let Ok(cnt) = count_tools_in_group(conn, &g.id) {
                    map.insert(g.id.clone(), cnt);
                }
            }
            map
        })
    });

    let mut show_new_group = use_signal(|| false);
    let mut new_group_name = use_signal(String::new);
    let mut new_group_desc = use_signal(String::new);
    let mut deleting_group = use_signal(|| Option::<String>::None);
    let mut editing_group = use_signal(|| Option::<String>::None);
    let mut edit_group_tools: Signal<Vec<String>> = use_signal(Vec::new);

    let all_tools = use_memo(move || {
        crate::db::with_db(|conn| {
            let mut stmt = conn.prepare(
                "SELECT id, name, description, schema_json, read_only, source, mcp_server, created_at
                 FROM tools ORDER BY name"
            ).ok()?;
            let rows = stmt.query_map([], |row| {
                Ok(ToolRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    description: row.get(2)?,
                    schema_json: row.get(3)?,
                    read_only: row.get::<_, i32>(4)? != 0,
                    source: row.get(5)?,
                    mcp_server: row.get(6)?,
                    created_at: row.get(7)?,
                })
            }).ok()?;
            let mut result = Vec::new();
            for r in rows {
                if let Ok(t) = r { result.push(t); }
            }
            Some(result)
        })
    });

    let do_add_group = move |_| {
        let name = new_group_name.read().trim().to_string();
        if name.is_empty() { return; }
        let desc = new_group_desc.read().trim().to_string();
        let id = format!("grp-{}", name.to_lowercase().replace(' ', "-"));
        let now = chrono::Local::now().to_rfc3339();
        let (grps, map) = crate::db::with_db(|conn| {
            let max_order: i32 = {
                let grps = list_groups(conn).unwrap_or_default();
                grps.iter().map(|g| g.sort_order).max().unwrap_or(0) + 1
            };
            let row = ToolGroupRow { id: id.clone(), name, description: desc, sort_order: max_order, created_at: now };
            if let Err(e) = insert_group(conn, &row) {
                tracing::error!(target:"db", error=%e, "insert tool group");
            }
            let grps = list_groups(conn).unwrap_or_default();
            let mut map = std::collections::HashMap::new();
            for g in &grps {
                if let Ok(cnt) = count_tools_in_group(conn, &g.id) {
                    map.insert(g.id.clone(), cnt);
                }
            }
            (grps, map)
        });
        groups.set(grps.clone());
        group_tool_count.set(map);
        groups_cache.set(grps);
        show_new_group.set(false);
        new_group_name.set(String::new());
        new_group_desc.set(String::new());
    };

    let mut do_delete_group = move |id: String| {
        let (grps, map) = crate::db::with_db(|conn| {
            if let Err(e) = delete_group(conn, &id) {
                tracing::error!(target:"db", error=%e, "delete tool group");
            }
            let grps = list_groups(conn).unwrap_or_default();
            let mut map = std::collections::HashMap::new();
            for g in &grps {
                if let Ok(cnt) = count_tools_in_group(conn, &g.id) {
                    map.insert(g.id.clone(), cnt);
                }
            }
            (grps, map)
        });
        groups.set(grps.clone());
        group_tool_count.set(map);
        groups_cache.set(grps);
        deleting_group.set(None);
    };

    let mut open_edit = move |group_id: String| {
        let tools = crate::db::with_db(|conn| {
            list_tools_in_group(conn, &group_id)
                .unwrap_or_default()
                .into_iter()
                .map(|t| t.id)
                .collect()
        });
        edit_group_tools.set(tools);
        editing_group.set(Some(group_id));
    };

    // 预计算迭代数据
    let current_tools = tools_data.read().0.clone();
    let total_count = tools_data.read().1;
    let total_pages = if total_count == 0 { 1 } else { ((total_count as f64) / (page_size as f64)).ceil() as i64 };
    let grp_opts = groups_cache.read().clone();
    let cur_filter = filter_group.read().clone();
    let cur_search = search_text.read().clone();
    let grps = groups.read().clone();
    let cnt_map = group_tool_count.read().clone();
    let del_id = deleting_group.read().clone();
    let group_items: Vec<GroupItem> = grps.iter().map(|g| GroupItem {
        id: g.id.clone(),
        name: g.name.clone(),
        description: g.description.clone(),
        tool_count: cnt_map.get(&g.id).copied().unwrap_or(0),
        is_deleting: del_id.as_deref() == Some(&g.id),
    }).collect();
    let edit_gid = editing_group();
    let edit_tools_raw = all_tools.read().clone().unwrap_or_default();
    let edit_selected = edit_group_tools.read().clone();
    let edit_tool_items: Vec<EditToolItem> = edit_tools_raw.iter().map(|t| EditToolItem {
        id: t.id.clone(),
        name: t.name.clone(),
        description: t.description.clone(),
        is_in_group: edit_selected.contains(&t.id),
    }).collect();
    let eg_name: String = grps.iter()
        .find(|g| Some(&g.id) == edit_gid.as_ref())
        .map(|g| g.name.clone())
        .unwrap_or_default();

    // 构建编辑弹窗（在 rsx! 外部以避免复杂嵌套）
    let edit_modal = if let Some(ref egid) = edit_gid {
        let gid = egid.clone();
        let items = edit_tool_items.clone();
        let gname = eg_name.clone();
        let mut es = editing_group;
        let mut egt = edit_group_tools;
        let mut grps_sig = groups;
        let mut gtc_sig = group_tool_count;
        let mut gc_sig = groups_cache;
        Some(rsx! {
            div {
                class: "settings-modal-backdrop",
                onclick: move |_| { es.set(None); egt.set(Vec::new()); },
                div {
                    class: "settings-modal-panel",
                    style: "width: min(92vw, 600px);",
                    onclick: move |evt| evt.stop_propagation(),
                    div { class: "settings-modal-header",
                        span { class: "settings-modal-title", "edit group: {gname}" }
                        button {
                            class: "settings-modal-close",
                            onclick: move |_| { es.set(None); egt.set(Vec::new()); },
                            "×"
                        }
                    }
                    div { class: "settings-modal-body",
                        div { class: "tools-edit-group-hint",
                            "check the tools you want in this group, uncheck to remove"
                        }
                        div { class: "tools-edit-group-list",
                            {items.iter().map(|item| {
                                let iid = item.id.clone();
                                let cid = gid.clone();
                                let is_in = item.is_in_group;
                                let mut egt2 = egt;

                                rsx! {
                                    div {
                                        class: if is_in { "tools-edit-group-item is-checked" } else { "tools-edit-group-item" },
                                        onclick: move |_| {
                                            let (grps, map) = crate::db::with_db(|conn| {
                                                if is_in {
                                                    let _ = remove_tool_from_group(conn, &cid, &iid);
                                                } else {
                                                    let _ = add_tool_to_group(conn, &cid, &iid, 0);
                                                }
                                                let grps = crate::db::metadata::tool::list_groups(conn).unwrap_or_default();
                                                let mut map = std::collections::HashMap::new();
                                                for g in &grps {
                                                    if let Ok(cnt) = crate::db::metadata::tool::count_tools_in_group(conn, &g.id) {
                                                        map.insert(g.id.clone(), cnt);
                                                    }
                                                }
                                                (grps, map)
                                            });
                                            // 更新工具列表信号
                                            if is_in {
                                                let mut list = egt2.write();
                                                list.retain(|t| t != &iid);
                                            } else {
                                                egt2.write().push(iid.clone());
                                            }
                                            grps_sig.set(grps.clone());
                                            gtc_sig.set(map);
                                            gc_sig.set(crate::db::with_db(|conn| {
                                                crate::db::metadata::tool::list_groups(conn).unwrap_or_default()
                                            }));
                                        },
                                        input {
                                            r#type: "checkbox",
                                            checked: is_in,
                                            oninput: move |_| {},
                                            style: "pointer-events: none;",
                                        }
                                        div { class: "tools-edit-group-item-info",
                                            span { class: "tools-edit-group-item-name", "{item.name}" }
                                            span { class: "tools-edit-group-item-desc", "{item.description}" }
                                        }
                                    }
                                }
                            })}
                        }
                    }
                    div { class: "settings-modal-header",
                        style: "justify-content: flex-end; border-top: 1px solid var(--color-rule); padding: var(--space-sm) var(--space-lg);",
                        button {
                            class: "btn btn-send",
                            style: "padding: 4px 16px; font-size: 11px;",
                            onclick: move |_| { es.set(None); egt.set(Vec::new()); },
                            "done"
                        }
                    }
                }
            }
        })
    } else {
        None
    };

    rsx! {
        div { class: "tools-panel",
            div { class: "settings-header",
                h2 { class: "settings-title", "tools" }
                span { class: "settings-subtitle", "view tools and manage groups" }
            }

            div { class: "tools-tabs",
                button {
                    class: if subtab() == ToolsSubTab::Tools { "tools-tab is-active" } else { "tools-tab" },
                    onclick: move |_| { subtab.set(ToolsSubTab::Tools); page.set(0); },
                    "tools"
                }
                button {
                    class: if subtab() == ToolsSubTab::Groups { "tools-tab is-active" } else { "tools-tab" },
                    onclick: move |_| subtab.set(ToolsSubTab::Groups),
                    "groups"
                }
            }

            match subtab() {
                ToolsSubTab::Tools => {
                    rsx! {
                        div { class: "settings-section",
                            div { class: "tools-toolbar",
                                Dropdown {
                                    value: cur_filter.clone().unwrap_or_default(),
                                    options: {
                                        let mut opts = vec![DropdownOption { value: String::new(), label: "all tools".into() }];
                                        for g in &grp_opts {
                                            opts.push(DropdownOption { value: g.id.clone(), label: g.name.clone() });
                                        }
                                        opts
                                    },
                                    placeholder: "all tools",
                                    onchange: move |val: String| {
                                        if val.is_empty() { filter_group.set(None); }
                                        else { filter_group.set(Some(val)); }
                                        page.set(0);
                                    },
                                }
                                input {
                                    class: "settings-input",
                                    style: "width: 200px;",
                                    placeholder: "search by name…",
                                    value: cur_search,
                                    oninput: move |evt| { search_text.set(evt.value()); page.set(0); },
                                }
                            }

                            if current_tools.is_empty() {
                                div { class: "provider-empty-state", span { "no tools found" } }
                            } else {
                                div { class: "tools-results-scroll",
                                    div { class: "tools-table-wrap",
                                        table { class: "tools-table",
                                        thead { tr {
                                            th { style: "width: 22%;", "name" } th { style: "width: 30%;", "description" } th { style: "width: 28%;", "source" } th { style: "width: 20%;", "read-only" }
                                        } }
                                        tbody {
                                            for tool in &current_tools {
                                                tr {
                                                    td { span { class: "tools-tool-name", "{tool.name}" } }
                                                    td { span { class: "tools-tool-desc", "{tool.description}" } }
                                                    td {
                                                        span {
                                                            class: if tool.source == "builtin" { "tools-source-tag" } else { "tools-source-tag tools-source-mcp" },
                                                            "{tool.source}"
                                                        }
                                                    }
                                                    td {
                                                        if tool.read_only { span { class: "tools-bool-yes", "yes" } }
                                                        else { span { class: "tools-bool-no", "no" } }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }

                                div { class: "tools-pagination",
                                    button {
                                        class: "btn btn-cancel",
                                        style: "padding: 2px 10px; font-size: 10px;",
                                        disabled: page() == 0,
                                        onclick: move |_| page.set((page() - 1).max(0)),
                                        "< prev"
                                    }
                                    span { class: "tools-page-info", "page {page() + 1} of {total_pages}" }
                                    button {
                                        class: "btn btn-cancel",
                                        style: "padding: 2px 10px; font-size: 10px;",
                                        disabled: page() + 1 >= total_pages,
                                        onclick: move |_| page.set((page() + 1).min(total_pages - 1)),
                                        "next >"
                                    }
                                }
                                }
                            }
                        }
                    }
                }

                ToolsSubTab::Groups => {
                    rsx! {
                        div { class: "settings-section",
                            if show_new_group() {
                                div { class: "tools-new-group-form",
                                    div { class: "settings-field",
                                        label { class: "settings-field-label", "group name" }
                                        input { class: "settings-input", placeholder: "my group", value: "{new_group_name}", oninput: move |evt| new_group_name.set(evt.value()) }
                                    }
                                    div { class: "settings-field",
                                        label { class: "settings-field-label", "description (optional)" }
                                        input { class: "settings-input", placeholder: "describe this group", value: "{new_group_desc}", oninput: move |evt| new_group_desc.set(evt.value()) }
                                    }
                                    div { class: "tools-new-group-actions",
                                        button { class: "btn btn-cancel", onclick: move |_| { show_new_group.set(false); new_group_name.set(String::new()); new_group_desc.set(String::new()); }, "cancel" }
                                        button { class: "btn btn-send", onclick: do_add_group, "create group" }
                                    }
                                }
                            } else {
                                div { class: "settings-field",
                                    button { class: "btn btn-new-chat", onclick: move |_| show_new_group.set(true), "+ new group" }
                                }
                            }

                            if group_items.is_empty() {
                                div { class: "provider-empty-state", span { "no tool groups yet" } }
                            } else {
                                div { class: "tools-group-list",
                                    for gi in &group_items {
                                        div { class: "tools-group-card",
                                            div { class: "tools-group-card-header",
                                                div { class: "tools-group-card-info",
                                                    span { class: "tools-group-card-name", "{gi.name}" }
                                                    span { class: "tools-group-card-count", "{gi.tool_count} tools" }
                                                }
                                                div { class: "tools-group-card-actions",
                                                    button {
                                                        class: "tools-group-card-btn",
                                                        onclick: { let g = gi.id.clone(); move |_| open_edit(g.clone()) },
                                                        "edit"
                                                    }
                                                    if gi.is_deleting {
                                                        span { class: "tools-group-card-confirm",
                                                            span { "delete?" }
                                                            button {
                                                                class: "tools-group-card-btn danger",
                                                                onclick: { let g = gi.id.clone(); move |_| do_delete_group(g.clone()) },
                                                                "yes"
                                                            }
                                                            button {
                                                                class: "tools-group-card-btn",
                                                                onclick: move |_| deleting_group.set(None),
                                                                "no"
                                                            }
                                                        }
                                                    } else {
                                                        button {
                                                            class: "tools-group-card-btn danger",
                                                            onclick: { let g = gi.id.clone(); move |_| deleting_group.set(Some(g.clone())) },
                                                            "delete"
                                                        }
                                                    }
                                                }
                                            }
                                            if !gi.description.is_empty() {
                                                div { class: "tools-group-card-desc", "{gi.description}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            {edit_modal}
        }
    }
}
