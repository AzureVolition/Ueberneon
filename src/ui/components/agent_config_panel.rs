// ── Agent 配置管理面板 ──

use dioxus::prelude::*;

use crate::db::metadata::agent_config::{self, AgentConfigRow};
use crate::db::metadata::tool::{self, ToolGroupRow};

/// 将 markdown 转为 html
fn markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(md, pulldown_cmark::Options::ENABLE_TABLES);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    html
}
use crate::db::metadata::provider_instance::{self, ProviderInstanceRow};
use crate::db::metadata::provider::{self, ProviderRow};
use crate::ui::components::dropdown::{Dropdown, DropdownOption};

fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let pid = std::process::id();
    format!("acfg-{ts:x}-{pid:x}")
}

/// Agent 类型选项
const AGENT_TYPES: &[(&str, &str)] = &[
    ("InBuilt", "InBuilt — 内置助手"),
    ("Custom", "Custom — 自定义助手"),
    ("SubAgent", "SubAgent — 子 Agent"),
];

#[component]
pub fn AgentConfigPanel(filter_agent_type: String, readonly: bool, edit_mode: String, on_change: EventHandler<()>) -> Element {
    // ── DB 数据 ──
    let mut configs: Signal<Vec<AgentConfigRow>> = use_signal(|| {
        crate::db::with_db(|conn| agent_config::list_by_type(conn, &filter_agent_type).unwrap_or_default())
    });
    let mut instances: Signal<Vec<ProviderInstanceRow>> = use_signal(|| {
        crate::db::with_db(|conn| provider_instance::list_all(conn).unwrap_or_default())
    });
    let mut providers_cache: Signal<Vec<ProviderRow>> = use_signal(|| {
        crate::db::with_db(|conn| provider::list_all(conn).unwrap_or_default())
    });
    let mut models_cache: Signal<std::collections::HashMap<String, Vec<String>>> = use_signal(|| {
        crate::db::with_db(|conn| {
            let mut map = std::collections::HashMap::new();
            if let Ok(providers) = provider::list_all(conn) {
                for p in &providers {
                    if let Ok(models) = provider::list_models(conn, &p.id) {
                        map.insert(p.id.clone(), models);
                    }
                }
            }
            map
        })
    });

    // ── 编辑状态 ──
    let mut show_editor = use_signal(|| false);
    let mut editing_id = use_signal(|| Option::<String>::None);
    let mut edit_name = use_signal(String::new);
    let mut edit_provider_inst = use_signal(String::new);
    let mut edit_model = use_signal(String::new);
    let mut edit_system_prompt = use_signal(String::new);
    let mut edit_temperature = use_signal(|| 0.7f64);
    let mut edit_max_tokens = use_signal(|| String::new());
    let mut edit_tools: Signal<std::collections::HashSet<String>> = use_signal(std::collections::HashSet::new);
    let mut edit_groups: Signal<std::collections::HashSet<String>> = use_signal(std::collections::HashSet::new);
    let mut show_group_selector = use_signal(|| false);
    let mut deleting = use_signal(|| Option::<String>::None);
    let mut viewing_prompt = use_signal(|| Option::<(String, String)>::None);

    // ── 加载工具组列表 ──
    let all_groups: Vec<ToolGroupRow> = crate::db::with_db(|conn| {
        tool::list_groups(conn).unwrap_or_default()
    });

    // ── 工具组 → 工具名查找缓存 ──
    let group_tools_cache: std::collections::HashMap<String, Vec<String>> = {
        crate::db::with_db(|conn| {
            let mut map = std::collections::HashMap::new();
            for g in &all_groups {
                if let Ok(tools) = tool::list_tools_in_group(conn, &g.id) {
                    map.insert(g.id.clone(), tools.into_iter().map(|t| t.name).collect());
                }
            }
            map
        })
    };

    let all_configs: Vec<AgentConfigRow> = configs.read().clone()
        .into_iter()
        .filter(|c| c.agent_type == filter_agent_type)
        .collect();
    let all_instances = instances.read().clone();
    let all_providers = providers_cache.read().clone();
    let all_models = models_cache.read().clone();

    // ── 辅助：找到 instance 对应的 provider name ──
    let provider_name_for_instance = |inst_id: &str| -> String {
        let inst = all_instances.iter().find(|i| i.id == inst_id);
        match inst {
            Some(i) => all_providers.iter().find(|p| p.id == i.provider_id)
                .map(|p| p.name.clone()).unwrap_or_else(|| i.provider_id.clone()),
            None => String::new(),
        }
    };

    let provider_id_for_instance = |inst_id: &str| -> String {
        all_instances.iter()
            .find(|i| i.id == inst_id)
            .map(|i| i.provider_id.clone())
            .unwrap_or_default()
    };

    // ── 开始编辑 ──
    let mut start_edit = {
        move |cfg: Option<&AgentConfigRow>| {
            show_editor.set(true);
            if let Some(c) = cfg {
                editing_id.set(Some(c.id.clone()));
                edit_name.set(c.name.clone());
                edit_provider_inst.set(c.provider_instance_id.clone());
                edit_model.set(c.model.clone());
                edit_system_prompt.set(c.system_prompt.clone());
                edit_temperature.set(c.temperature);
                edit_max_tokens.set(c.max_tokens.map(|v| v.to_string()).unwrap_or_default());
                // 解析 tools JSON
                let tools_set: std::collections::HashSet<String> =
                    serde_json::from_str(&c.tools).unwrap_or_default();
                edit_tools.set(tools_set);
                // 加载已保存的工具组关联
                let ids = crate::db::with_db(|conn| {
                    agent_config::load_group_ids(conn, &c.id).unwrap_or_default()
                });
                edit_groups.set(ids.into_iter().collect());
            } else {
                editing_id.set(None);
                edit_name.set(String::new());
                edit_provider_inst.set(String::new());
                edit_model.set(String::new());
                edit_system_prompt.set(String::new());
                edit_temperature.set(0.7);
                edit_max_tokens.set(String::new());
                edit_tools.set(std::collections::HashSet::new());
                edit_groups.set(std::collections::HashSet::new());
            }
        }
    };

    let filter_type = filter_agent_type.clone();
    let filter_type_clone = filter_type.clone();

    // ── 保存 ──
    let mut do_save = {
        move |_| {
            let now = chrono::Local::now().to_rfc3339();
            let id = editing_id.read().clone().unwrap_or_else(gen_id);
            let name = edit_name.read().trim().to_string();
            if name.is_empty() { return; }
            //let agent_type = edit_agent_type.read().clone();
            let provider_inst = edit_provider_inst.read().clone();
            let model = edit_model.read().clone();
            let system_prompt = edit_system_prompt.read().clone();
            let temperature = *edit_temperature.read();
            let max_tokens = {
                let v = edit_max_tokens.read().trim().to_string();
                if v.is_empty() { None } else { v.parse::<u32>().ok() }
            };
            let tools = {
                // 展开选中的工具组 → 工具名列表 → 去重
                let mut tool_set: std::collections::HashSet<String> = std::collections::HashSet::new();
                let group_ids: Vec<String> = edit_groups.read().iter().cloned().collect();
                crate::db::with_db(|conn| {
                    for gid in &group_ids {
                        if let Ok(tools) = crate::db::metadata::tool::list_tools_in_group(conn, gid) {
                            for t in &tools {
                                tool_set.insert(t.name.clone());
                            }
                        }
                    }
                });
                serde_json::to_string(&tool_set.into_iter().collect::<Vec<_>>()).unwrap_or_else(|_| "[]".to_string())
            };
            let is_new = editing_id.read().is_none();

            // 从 provider instance 获取 base_url 和 api_key
            let (base_url, api_key) = {
                let inst_id = edit_provider_inst.read().clone();
                crate::db::with_db(|conn| {
                    let inst = crate::db::metadata::provider_instance::get(conn, &inst_id)
                        .ok().flatten();
                    let (raw_key, prov_id) = match inst {
                        Some(ref i) => (i.api_key.clone(), i.provider_id.clone()),
                        None => (String::new(), String::new()),
                    };
                    let url = crate::db::metadata::provider::get(conn, &prov_id)
                        .ok().flatten().map(|p| p.base_url).unwrap_or_default();
                    (url, raw_key)
                })
            };

            let row = AgentConfigRow {
                id: if is_new { gen_id() } else { editing_id.read().clone().unwrap() },
                name,
                agent_type: filter_type.clone(),
                provider_instance_id: provider_inst,
                model,
                base_url,
                api_key,
                system_prompt,
                temperature,
                max_tokens,
                tools,
                created_at: if is_new { now.clone() } else { String::new() },
                updated_at: now,
            };

            let row_id = row.id.clone();
            crate::db::with_db(|conn| {
                if is_new {
                    if let Err(e) = agent_config::insert(conn, &row) { tracing::error!(target:"db", error=%e, "insert agent config"); }
                } else {
                    if let Err(e) = agent_config::update(conn, &row) { tracing::error!(target:"db", error=%e, "update agent config"); }
                }
                let group_ids: Vec<String> = edit_groups.read().iter().cloned().collect();
                if let Err(e) = agent_config::save_groups(conn, &row_id, &group_ids) {
                    tracing::error!(target:"db", error=%e, "save agent config groups");
                }
            });
            configs.set(crate::db::with_db(|conn| {
                agent_config::list_by_type(conn, &filter_type).unwrap_or_default()
            }));
            show_editor.set(false);
            editing_id.set(None);
            on_change.call(());
        }
    };

    let is_new = editing_id().is_none();
    let is_provider_only = edit_mode == "provider_only";
    let sel_gcache = group_tools_cache.clone();
    let sel_grps = all_groups.clone();
    let sel_selected = edit_groups.read().clone();
    let sel_count = sel_selected.len();
    let total_tools: usize = sel_selected.iter()
        .filter_map(|id| sel_gcache.get(id))
        .map(|v| v.len())
        .sum();

    rsx! {
        div { class: "settings-section",
            if show_editor() && (!readonly || is_provider_only) {
                div {
                    class: "settings-modal-backdrop",
                    onclick: move |_| { show_editor.set(false); editing_id.set(None); edit_name.set(String::new()); },
                    div {
                        class: "settings-modal-panel",
                        onclick: move |evt| evt.stop_propagation(),
                        div { class: "settings-modal-header",
                            span { class: "settings-modal-title",
                                if is_new { "new agent config" } else { "edit agent config" }
                            }
                            button {
                                class: "settings-modal-close",
                                onclick: move |_| { show_editor.set(false); editing_id.set(None); edit_name.set(String::new()); },
                                "✕"
                            }
                        }
                        div { class: "settings-modal-body",
                            div { class: "provider-form",
                    if !is_provider_only {
                        div { class: "settings-field",
                            label { class: "settings-field-label", "name" }
                            input {
                                class: "settings-input",
                                placeholder: "my agent config",
                                value: "{edit_name}",
                                oninput: move |evt| edit_name.set(evt.value()),
                            }
                        }
                    }
                    div { class: "settings-field",
                        label { class: "settings-field-label", "provider instance" }
                        Dropdown {
                            value: edit_provider_inst(),
                            options: {
                                let mut opts = vec![DropdownOption { value: String::new(), label: "— select —".into() }];
                                for inst in &all_instances {
                                    let pn = provider_name_for_instance(&inst.id);
                                    opts.push(DropdownOption {
                                        value: inst.id.clone(),
                                        label: format!("{} ({})", inst.alias, pn),
                                    });
                                }
                                opts
                            },
                            onchange: move |val| {
                                edit_provider_inst.set(val);
                                edit_model.set(String::new());
                            },
                        }
                    }
                    div { class: "settings-field",
                        label { class: "settings-field-label", "model" }
                        Dropdown {
                            value: edit_model(),
                            options: {
                                let pid = provider_id_for_instance(&edit_provider_inst());
                                let models = all_models.get(&pid).cloned().unwrap_or_default();
                                let mut opts = vec![DropdownOption { value: String::new(), label: "— select —".into() }];
                                for m in &models {
                                    opts.push(DropdownOption { value: m.clone(), label: m.clone() });
                                }
                                opts
                            },
                            onchange: move |val| edit_model.set(val),
                        }
                    }
                    if !is_provider_only {
                        div { class: "settings-field",
                            label { class: "settings-field-label", "system prompt" }
                            textarea {
                                class: "settings-input",
                                style: "min-height: 120px; resize: vertical; font-family: var(--font-mono, monospace); font-size: 12px;",
                                placeholder: "You are a helpful assistant...",
                                value: "{edit_system_prompt}",
                                oninput: move |evt| edit_system_prompt.set(evt.value()),
                            }
                        }
                        div { class: "settings-field",
                            label { class: "settings-field-label", "temperature" }
                            div { style: "display: flex; align-items: center; gap: 8px;",
                                input {
                                    r#type: "range",
                                    min: "0.0",
                                    max: "2.0",
                                    step: "0.1",
                                    value: "{edit_temperature}",
                                    oninput: move |evt| {
                                        if let Ok(v) = evt.value().parse::<f64>() {
                                            edit_temperature.set(v);
                                        }
                                    },
                                    style: "flex: 1;",
                                }
                                span { "{edit_temperature:.1}" }
                            }
                        }
                        div { class: "settings-field",
                            label { class: "settings-field-label", "max tokens" }
                            input {
                                class: "settings-input",
                                r#type: "number",
                                placeholder: "4096",
                                value: "{edit_max_tokens}",
                                oninput: move |evt| edit_max_tokens.set(evt.value()),
                            }
                        }
                        div { class: "settings-field",
                            label { class: "settings-field-label", "tool groups" }
                            div { class: "tools-group-selector-bar",
                                // 显示已选组
                                {all_groups.iter().filter_map(|g| {
                                    if edit_groups.read().contains(&g.id) {
                                        let gname = g.name.clone();
                                        let cnt = group_tools_cache.get(&g.id).map(|v| v.len()).unwrap_or(0);
                                        Some(rsx! {
                                            span { class: "tools-group-pill",
                                                "{gname} ({cnt})"
                                            }
                                        })
                                    } else {
                                        None
                                    }
                                })}
                                if edit_groups.read().is_empty() {
                                    span { class: "tools-group-pill is-empty", "(none = all tools)" }
                                }
                                button {
                                    class: "tools-group-select-btn",
                                    onclick: move |_| show_group_selector.set(true),
                                    "select groups ⋮"
                                }
                            }
                        }
                    }
                    div { class: "provider-custom-form-actions",
                        button {
                            class: "btn btn-cancel",
                            onclick: move |_| {
                                show_editor.set(false);
                                editing_id.set(None);
                                edit_name.set(String::new());
                            },
                            "cancel"
                        }
                        button {
                            class: "btn btn-send",
                            onclick: move |_| do_save(()),
                            if editing_id().is_some() { "save" } else { "create" }
                        }
                    }
                }
                    }
                    }
                    }
            } else {
                // ── 列表 ──
                if !readonly {
                    button {
                        class: "provider-add-btn",
                        onclick: move |_| {
                            start_edit(None);
                        },
                        "+ new agent config"
                    }
                }

                for cfg in &all_configs {
                    {
                        let cfg_id = cfg.id.clone();
                        let show_confirm = deleting() == Some(cfg_id.clone());
                        let pname = provider_name_for_instance(&cfg.provider_instance_id);

                        rsx! {
                            div {
                                class: "provider-block",
                                div { class: "provider-block-header",
                                    div { class: "provider-block-title-row",
                                        div { class: "provider-block-info",
                                            div { class: "provider-block-name-row",
                                                span { class: "provider-block-name", "{cfg.name}" }
                                                span { class: "provider-badge-kind", "{cfg.agent_type}" }
                                            }
                                            span { class: "provider-block-url",
                                                if !pname.is_empty() {
                                                    "{pname} · {cfg.model}"
                                                } else {
                                                    "{cfg.model}"
                                                }
                                            }
                                        }
                                    }
                                }
                                div { class: "provider-block-body",
                                    div { class: "provider-block-row",
                                        div { class: "provider-block-row-left",
                                            span { class: "provider-block-label", "temperature" }
                                            span { "{cfg.temperature:.1}" }
                                        }
                                        if !readonly || is_provider_only {
                                            button {
                                                class: "provider-block-action-btn",
                                                onclick: {
                                                    let c = cfg.clone();
                                                    move |_| start_edit(Some(&c))
                                                },
                                                "edit"
                                            }
                                        }
                                    }
                                    // ── tools ──
                                    {
                                        let tools_json = &cfg.tools;
                                        let tools_arr: Vec<String> = if tools_json.is_empty() || tools_json == "[]" {
                                            vec!["(all tools)".to_string()]
                                        } else {
                                            serde_json::from_str(tools_json).unwrap_or_default()
                                        };
                                        let tools_label = format!("tools ({})", tools_arr.len());
                                        rsx! {
                                            div { class: "provider-block-row",
                                                div { class: "provider-block-row-left provider-block-row-tools",
                                                    span { class: "provider-block-label", "{tools_label}" }
                                                    div { class: "model-pill-grid",
                                                        for tool in &tools_arr {
                                                            span { class: "tool-pill", "{tool}" }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    if !cfg.system_prompt.is_empty() {
                                        div { class: "provider-block-row",
                                            div { class: "provider-block-row-left",
                                                span { class: "provider-block-label", "prompt" }
                                                span { style: "font-size: 11px; color: var(--color-ink-2, #888); max-width: 200px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                                    "{cfg.system_prompt}"
                                                }
                                            }
                                            button {
                                                class: "provider-block-action-btn",
                                                onclick: {
                                                    let c = cfg.clone();
                                                    move |_| viewing_prompt.set(Some((c.name.clone(), c.system_prompt.clone())))
                                                },
                                                "view prompt"
                                            }
                                        }
                                    }
                                }
                                // ── Prompt 查看弹窗 ──
                                {
                                    let show = viewing_prompt.read().clone();
                                    match show {
                                        Some((ref _cfg_name, ref content)) => {
                                            rsx! {
                                                div {
                                                    class: "settings-modal-backdrop",
                                                    onclick: move |_| viewing_prompt.set(None),
                                                    div {
                                                        class: "settings-modal-panel prompt-modal",
                                                        onclick: move |evt| evt.stop_propagation(),
                                                        div { class: "settings-modal-header",
                                                            span { class: "settings-modal-title", "system prompt" }
                                                        }
                                                        div { class: "settings-modal-body prompt-modal-body",
                                                            div { class: "prompt-content",
                                                                dangerous_inner_html: markdown_to_html(content)
                                                            }
                                                        }
                                                        div { class: "prompt-modal-footer",
                                                            button {
                                                                class: "btn btn-send",
                                                                style: "padding: 4px 16px; font-size: 11px;",
                                                                onclick: move |_| viewing_prompt.set(None),
                                                                "close"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                        None => rsx! {}
                                    }
                                }
                                if !readonly {
                                    div { class: "provider-block-footer",
                                        if show_confirm {
                                            div { class: "provider-block-confirm-delete",
                                                span { class: "provider-block-confirm-text",
                                                    "delete \"{cfg.name}\"? this cannot be undone."
                                                }
                                                div { class: "provider-block-confirm-actions",
                                                    button { class: "btn btn-cancel", style: "padding: 2px 10px; font-size: 10px;", onclick: move |_| deleting.set(None), "cancel" }
                                                    button { class: "btn btn-send", style: "padding: 2px 10px; font-size: 10px; background: var(--color-error); color: var(--color-paper);", onclick: { let sid = cfg.id.clone(); let mut dlt = deleting; let mut cfg_sig = configs; let ft = filter_type_clone.clone(); let oc = on_change; move |_| { crate::db::with_db(|conn| { if let Err(e) = agent_config::delete(conn, &sid) { tracing::error!(target:"db", error=%e, "delete agent config"); } }); cfg_sig.set(crate::db::with_db(|conn| agent_config::list_by_type(conn, &ft).unwrap_or_default())); dlt.set(None); oc.call(()); } }, "confirm delete" }
                                                }
                                            }
                                        } else {
                                            button {
                                                class: "provider-block-delete-btn",
                                                onclick: { let sid = cfg.id.clone(); move |_| deleting.set(Some(sid.clone())) },
                                                "delete"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                if all_configs.is_empty() {
                    div { class: "provider-empty-state",
                        if readonly {
                            span { "no sub agents" }
                        } else {
                            span { "no agent configs yet" }
                            span { style: "color: var(--color-ink-4); font-size: var(--text-sm);", "click \"+ new agent config\" to create your first configuration" }
                        }
                    }
                }
        }

        // ── 工具组选择弹窗（表格视图）──
        if show_group_selector() {
            div {
                class: "settings-modal-backdrop",
                onclick: move |_| show_group_selector.set(false),
                div {
                        class: "settings-modal-panel",
                        style: "width: min(95vw, 720px);",
                        onclick: move |evt| evt.stop_propagation(),
                        div { class: "settings-modal-header",
                            span { class: "settings-modal-title", "select tool groups" }
                            button {
                                class: "settings-modal-close",
                                onclick: move |_| show_group_selector.set(false),
                                "×"
                            }
                        }
                        div { class: "settings-modal-body",
                            div { class: "tools-selector-table-wrap",
                                table { class: "tools-selector-table",
                                    thead { tr {
                                        th { style: "width: 30%;", "group" }
                                        th { "tools" }
                                        th { style: "width: 80px;", "count" }
                                    } }
                                    tbody {
                                        {sel_grps.iter().map(|g| {
                                            let gid = g.id.clone();
                                            let gname = g.name.clone();
                                            let gdesc = g.description.clone();
                                            let is_sel = sel_selected.contains(&gid);
                                            let tool_names = sel_gcache.get(&gid).cloned().unwrap_or_default();
                                            let cnt = tool_names.len();
                                            rsx! {
                                                tr {
                                                    class: if is_sel { "tools-selector-row is-selected" } else { "tools-selector-row" },
                                                    onclick: {
                                                        let gid2 = gid.clone();
                                                        move |_| {
                                                            let mut gs = edit_groups.write();
                                                            if gs.contains(&gid2) { gs.remove(&gid2); }
                                                            else { gs.insert(gid2.clone()); }
                                                        }
                                                    },
                                                    td {
                                                        span { class: "tools-selector-group-name", "{gname}" }
                                                        if !gdesc.is_empty() {
                                                            span { class: "tools-selector-group-desc", "{gdesc}" }
                                                        }
                                                    }
                                                    td {
                                                        div { class: "tools-selector-tool-list",
                                                            for tn in &tool_names {
                                                                span { class: "tools-selector-tool-tag", "{tn}" }
                                                            }
                                                        }
                                                    }
                                                    td {
                                                        span { class: "tools-selector-count", "{cnt}" }
                                                    }
                                                }
                                            }
                                        })}
                                    }
                                }
                            }
                            div { class: "tools-selector-summary",
                                span {
                                    "{sel_count} groups selected · {total_tools} tools enabled"
                                }
                                span { style: "color: var(--color-ink-4); font-size: 11px;",
                                    " (none = all tools)"
                                }
                            }
                        }
                        div { class: "settings-modal-header",
                            style: "justify-content: flex-end; border-top: 1px solid var(--color-rule); padding: var(--space-sm) var(--space-lg);",
                            button {
                                class: "btn btn-send",
                                style: "padding: 4px 20px; font-size: 12px;",
                                onclick: move |_| show_group_selector.set(false),
                                "done"
                            }
                        }
                    }
                }
            }
        }
    }
}
