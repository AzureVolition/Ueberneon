// ── Agent 配置管理面板 ──

use dioxus::prelude::*;

use crate::db::metadata::agent_config::{self, AgentConfigRow};
use crate::db::metadata::provider_instance::{self, ProviderInstanceRow};
use crate::db::metadata::provider::{self, ProviderRow};
use crate::ui::components::dropdown::{Dropdown, DropdownOption};

fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let pid = std::process::id();
    format!("acfg-{ts:x}-{pid:x}")
}

/// 内置工具列表
const ALL_TOOLS: &[(&str, &str)] = &[
    ("bash", "Bash — 执行 shell 命令"),
    ("read_only_bash", "ReadOnlyBash — 只读 shell"),
    ("bash_output", "BashOutput — 读取命令输出"),
    ("kill_shell", "KillShell — 终止 shell 进程"),
    ("read_file", "ReadFile — 读取文件"),
    ("edit_file", "EditFile — 编辑文件"),
    ("write_file", "WriteFile — 写入文件"),
    ("multi_edit", "MultiEdit — 批量编辑"),
    ("grep", "Grep — 搜索文件内容"),
    ("glob", "Glob — 搜索文件路径"),
    ("ls", "Ls — 列出目录"),
    ("code_index", "CodeIndex — 代码索引"),
    ("web_fetch", "WebFetch — 获取网页"),
];

/// Agent 类型选项
const AGENT_TYPES: &[(&str, &str)] = &[
    ("general", "General — 通用助手"),
    ("frontend-design", "Frontend Design — 前端设计"),
    ("library-design", "Library Design — 库设计"),
];

#[component]
pub fn AgentConfigPanel() -> Element {
    // ── DB 数据 ──
    let mut configs: Signal<Vec<AgentConfigRow>> = use_signal(|| {
        let conn = crate::db::get_db().lock().unwrap();
        agent_config::list_all(&conn).unwrap_or_default()
    });
    let mut instances: Signal<Vec<ProviderInstanceRow>> = use_signal(|| {
        let conn = crate::db::get_db().lock().unwrap();
        provider_instance::list_all(&conn).unwrap_or_default()
    });
    let mut providers_cache: Signal<Vec<ProviderRow>> = use_signal(|| {
        let conn = crate::db::get_db().lock().unwrap();
        provider::list_all(&conn).unwrap_or_default()
    });
    let mut models_cache: Signal<std::collections::HashMap<String, Vec<String>>> = use_signal(|| {
        let conn = crate::db::get_db().lock().unwrap();
        let mut map = std::collections::HashMap::new();
        if let Ok(providers) = provider::list_all(&conn) {
            for p in &providers {
                if let Ok(models) = provider::list_models(&conn, &p.id) {
                    map.insert(p.id.clone(), models);
                }
            }
        }
        map
    });

    // ── 编辑状态 ──
    let mut show_editor = use_signal(|| false);
    let mut editing_id = use_signal(|| Option::<String>::None);
    let mut edit_name = use_signal(String::new);
    let mut edit_agent_type = use_signal(|| "general".to_string());
    let mut edit_provider_inst = use_signal(String::new);
    let mut edit_model = use_signal(String::new);
    let mut edit_system_prompt = use_signal(String::new);
    let mut edit_temperature = use_signal(|| 0.7f64);
    let mut edit_max_tokens = use_signal(|| String::new());
    let mut edit_tools: Signal<std::collections::HashSet<String>> = use_signal(std::collections::HashSet::new);
    let mut deleting = use_signal(|| Option::<String>::None);

    let all_configs = configs.read().clone();
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
                edit_agent_type.set(c.agent_type.clone());
                edit_provider_inst.set(c.provider_instance_id.clone());
                edit_model.set(c.model.clone());
                edit_system_prompt.set(c.system_prompt.clone());
                edit_temperature.set(c.temperature);
                edit_max_tokens.set(c.max_tokens.map(|v| v.to_string()).unwrap_or_default());
                // 解析 tools JSON
                let tools_set: std::collections::HashSet<String> =
                    serde_json::from_str(&c.tools).unwrap_or_default();
                edit_tools.set(tools_set);
            } else {
                editing_id.set(None);
                edit_name.set(String::new());
                edit_agent_type.set("general".to_string());
                edit_provider_inst.set(String::new());
                edit_model.set(String::new());
                edit_system_prompt.set(String::new());
                edit_temperature.set(0.7);
                edit_max_tokens.set(String::new());
                edit_tools.set(std::collections::HashSet::new());
            }
        }
    };

    // ── 保存 ──
    let mut do_save = {
        move |_| {
            let now = chrono::Local::now().to_rfc3339();
            let id = editing_id.read().clone().unwrap_or_else(gen_id);
            let name = edit_name.read().trim().to_string();
            if name.is_empty() { return; }
            let agent_type = edit_agent_type.read().clone();
            let provider_inst = edit_provider_inst.read().clone();
            let model = edit_model.read().clone();
            let system_prompt = edit_system_prompt.read().clone();
            let temperature = *edit_temperature.read();
            let max_tokens = {
                let v = edit_max_tokens.read().trim().to_string();
                if v.is_empty() { None } else { v.parse::<u32>().ok() }
            };
            let tools = serde_json::to_string(&*edit_tools.read()).unwrap_or_else(|_| "[]".to_string());
            let is_new = editing_id.read().is_none();

            // 从 provider instance 获取 base_url 和 api_key
            let (base_url, api_key) = {
                let conn = crate::db::get_db().lock().unwrap();
                let inst = crate::db::metadata::provider_instance::get(&conn, &edit_provider_inst.read())
                    .ok().flatten();
                let (raw_key, prov_id) = match inst {
                    Some(ref i) => (i.api_key.clone(), i.provider_id.clone()),
                    None => (String::new(), String::new()),
                };
                let decoded_key = if !raw_key.is_empty() {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.decode(raw_key.as_bytes())
                        .ok().and_then(|v| String::from_utf8(v).ok())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                let url = crate::db::metadata::provider::get(&conn, &prov_id)
                    .ok().flatten().map(|p| p.base_url).unwrap_or_default();
                drop(conn);
                (url, decoded_key)
            };

            let row = AgentConfigRow {
                id: if is_new { gen_id() } else { editing_id.read().clone().unwrap() },
                name,
                agent_type,
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

            let conn = crate::db::get_db().lock().unwrap();
            if is_new {
                if let Err(e) = agent_config::insert(&conn, &row) { tracing::error!(target:"db", error=%e, "insert agent config"); }
            } else {
                if let Err(e) = agent_config::update(&conn, &row) { tracing::error!(target:"db", error=%e, "update agent config"); }
            }
            drop(conn);
            configs.set({
                let conn = crate::db::get_db().lock().unwrap();
                agent_config::list_all(&conn).unwrap_or_default()
            });
            show_editor.set(false);
            editing_id.set(None);
        }
    };

    // ── 删除 ──
    let mut do_delete = {
        move |id: String| {
            let conn = crate::db::get_db().lock().unwrap();
            if let Err(e) = agent_config::delete(&conn, &id) { tracing::error!(target:"db", error=%e, "delete agent config"); }
            drop(conn);
            configs.set({
                let conn = crate::db::get_db().lock().unwrap();
                agent_config::list_all(&conn).unwrap_or_default()
            });
            deleting.set(None);
        }
    };


    let is_new = editing_id().is_none();

    rsx! {
        div { class: "settings-section",
            if show_editor() {
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
                    div { class: "settings-field",
                        label { class: "settings-field-label", "name" }
                        input {
                            class: "settings-input",
                            placeholder: "my agent config",
                            value: "{edit_name}",
                            oninput: move |evt| edit_name.set(evt.value()),
                        }
                    }
                    div { class: "settings-field",
                        label { class: "settings-field-label", "agent type" }
                        Dropdown {
                            value: edit_agent_type(),
                            options: AGENT_TYPES.iter().map(|(key, label)| DropdownOption {
                                value: key.to_string(),
                                label: label.to_string(),
                            }).collect(),
                            onchange: move |val| edit_agent_type.set(val),
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
                        label { class: "settings-field-label", "enabled tools" }
                        div { class: "model-pill-grid",
                            for (key, label) in ALL_TOOLS {
                                {
                                    let is_checked = edit_tools.read().contains(*key);
                                    let skey = key.to_string();
                                    rsx! {
                                        button {
                                            class: if is_checked { "mode-pill is-active" } else { "mode-pill" },
                                            onclick: {
                                                let sk = skey.clone();
                                                move |_| {
                                                    let mut tools = edit_tools.write();
                                                    if tools.contains(&sk) {
                                                        tools.remove(&sk);
                                                    } else {
                                                        tools.insert(sk.clone());
                                                    }
                                                }
                                            },
                                            style: "font-size: 11px;",
                                            if is_checked { "✓ " } else { "" } "{key}"
                                        }
                                    }
                                }
                            }
                        }
                        div { style: "margin-top: 4px;",
                            span { style: "font-size: 11px; color: var(--color-ink-2, #888);",
                                "(empty = all tools enabled)"
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
                button {
                    class: "provider-add-btn",
                    onclick: move |_| {
                        start_edit(None);
                    },
                    "+ new agent config"
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
                                        button {
                                            class: "provider-block-action-btn",
                                            onclick: {
                                                let c = cfg.clone();
                                                move |_| start_edit(Some(&c))
                                            },
                                            "edit"
                                        }
                                    }
                                    if !cfg.system_prompt.is_empty() {
                                        div { class: "provider-block-row",
                                            div { class: "provider-block-row-left",
                                                span { class: "provider-block-label", "prompt" }
                                                span { style: "font-size: 11px; color: var(--color-ink-2, #888); max-width: 300px; overflow: hidden; text-overflow: ellipsis; white-space: nowrap;",
                                                    "{cfg.system_prompt}"
                                                }
                                            }
                                        }
                                    }
                                }
                                div { class: "provider-block-footer",
                                    if show_confirm {
                                        div { class: "provider-block-confirm-delete",
                                            span { class: "provider-block-confirm-text",
                                                "delete \"{cfg.name}\"? this cannot be undone."
                                            }
                                            div { class: "provider-block-confirm-actions",
                                                button { class: "btn btn-cancel", style: "padding: 2px 10px; font-size: 10px;", onclick: move |_| deleting.set(None), "cancel" }
                                                button { class: "btn btn-send", style: "padding: 2px 10px; font-size: 10px; background: var(--color-error); color: var(--color-paper);", onclick: { let sid = cfg.id.clone(); move |_| do_delete(sid.clone()) }, "confirm delete" }
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

                if all_configs.is_empty() {
                    div { class: "provider-empty-state",
                        span { "no agent configs yet" }
                        span { style: "color: var(--color-ink-4); font-size: var(--text-sm);", "click \"+ new agent config\" to create your first configuration" }
                    }
                }
            }
        }
    }
}
