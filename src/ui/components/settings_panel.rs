// ── Provider 实例配置面板 ──
//
// 只显示已接入的 provider 实例，每个实例块可删除 / 刷新模型 / 更改密钥。
// 添加实例时从 providers 表（预设）选择服务，再指定 alias + key。

use dioxus::prelude::*;

use crate::agent::{ActionMode, AgentMode};
use crate::db::metadata::agent_config;
use crate::db::metadata::provider::{self, ProviderRow};
use crate::db::metadata::provider_instance::{self, ProviderInstanceRow};
use crate::db::provider_presets::{self, ProviderPreset};
use crate::settings;
use crate::ui::components::agent_config_panel::AgentConfigPanel;
use crate::ui::components::dropdown::{Dropdown, DropdownOption};
use crate::ui::components::sql_panel::SqlPanel;
use crate::ui::state::SettingsTab;

fn encode_key(key: &str) -> String {
    use base64::Engine;
    if key.is_empty() {
        String::new()
    } else {
        base64::engine::general_purpose::STANDARD.encode(key.as_bytes())
    }
}

fn decode_key(encoded: &str) -> String {
    use base64::Engine;
    if encoded.is_empty() {
        String::new()
    } else {
        base64::engine::general_purpose::STANDARD
            .decode(encoded.as_bytes())
            .ok()
            .and_then(|v| String::from_utf8(v).ok())
            .unwrap_or_default()
    }
}

/// 生成唯一 ID
fn gen_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("inst-{ts:x}-{pid:x}")
}

#[component]
pub fn SettingsPanel(tab: SettingsTab, on_change: EventHandler<()>) -> Element {
    // ── DB 数据 ──
    let mut instances: Signal<Vec<ProviderInstanceRow>> = use_signal(|| {
        let conn = crate::db::get_db().lock().unwrap();
        provider_instance::list_all(&conn).unwrap_or_default()
    });
    let mut providers_cache: Signal<Vec<ProviderRow>> = use_signal(|| {
        let conn = crate::db::get_db().lock().unwrap();
        provider::list_all(&conn).unwrap_or_default()
    });
    let mut models_cache: Signal<std::collections::HashMap<String, Vec<String>>> =
        use_signal(|| {
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

    // ── UI state ──
    let mut editing_key_for = use_signal(|| Option::<String>::None);
    let mut key_input = use_signal(String::new);
    let mut refreshing = use_signal(|| Option::<String>::None);
    let mut fetch_errors: Signal<std::collections::HashMap<String, String>> =
        use_signal(std::collections::HashMap::new);
    let mut deleting = use_signal(|| Option::<String>::None);
    let mut show_add_form = use_signal(|| false);
    let mut add_step = use_signal(|| AddStep::SelectProvider);
    let mut add_provider_id = use_signal(String::new);
    let mut add_alias = use_signal(String::new);
    let mut add_key = use_signal(String::new);

    // 自定义 provider 字段
    let mut custom_id = use_signal(String::new);
    let mut custom_name = use_signal(String::new);
    let mut custom_url = use_signal(String::new);

    let ins = instances.read().clone();
    let provs = providers_cache.read().clone();
    let models = models_cache.read().clone();
    let errs = fetch_errors.read().clone();
    let adding = show_add_form();

    // ── 刷新模型 ──
    let mut do_refresh = {
        move |inst_id: String| {
            // 查找实例对应的 provider_id
            let conn = crate::db::get_db().lock().unwrap();
            let inst = provider_instance::get(&conn, &inst_id).ok().flatten();
            drop(conn);
            let (pid, key) = match inst {
                Some(i) => (i.provider_id, decode_key(&i.api_key)),
                None => return,
            };
            if key.is_empty() {
                fetch_errors
                    .write()
                    .insert(inst_id.clone(), "api key required".into());
                return;
            }
            refreshing.set(Some(inst_id.clone()));
            fetch_errors.write().remove(&inst_id);
            let pid2 = pid.clone();
            let key2 = key.clone();
            spawn(async move {
                let conn = crate::db::get_db().lock().unwrap();
                let prov = provider::get(&conn, &pid2).ok().flatten();
                drop(conn);
                match prov {
                    Some(p) => {
                        match crate::db::model_fetch::refresh_and_save(
                            &crate::db::get_db().lock().unwrap(),
                            &p,
                            &key2,
                        )
                        .await
                        {
                            Ok(models) => {
                                models_cache.write().insert(pid2.clone(), models);
                                fetch_errors.write().remove(&inst_id);
                            }
                            Err(e) => {
                                fetch_errors.write().insert(inst_id.clone(), e);
                            }
                        }
                    }
                    None => {
                        fetch_errors
                            .write()
                            .insert(inst_id.clone(), "provider not found".into());
                    }
                }
                refreshing.set(None);
            });
        }
    };

    // ── 删除实例 ──
    let mut do_delete = {
        move |inst_id: String| {
            let conn = crate::db::get_db().lock().unwrap();
            if let Err(e) = provider_instance::delete(&conn, &inst_id) { tracing::error!(target:"db", error=%e, "delete provider instance"); }
            drop(conn);
            // 刷新
            let conn = crate::db::get_db().lock().unwrap();
            instances.set(provider_instance::list_all(&conn).unwrap_or_default());
            fetch_errors.write().remove(&inst_id);
            deleting.set(None);
            on_change.call(());
        }
    };

    // ── 保存密钥 ──
    let mut do_save_key = {
        move |inst_id: String| {
            let val = key_input.read().trim().to_string();
            let encoded = encode_key(&val);
            let conn = crate::db::get_db().lock().unwrap();
            if let Err(e) = provider_instance::update_key(&conn, &inst_id, &encoded) { tracing::error!(target:"db", error=%e, "update provider key"); }
            // 同步更新所有使用该 provider instance 的 agent config 的 key
            conn.execute(
                    "UPDATE agent_configs SET api_key = ?1, updated_at = ?2 WHERE provider_instance_id = ?3",
                    rusqlite::params![encoded, chrono::Local::now().to_rfc3339(), inst_id],
                ).unwrap_or_else(|e| { tracing::error!(target:"db", error=%e, "update agent configs key."); 0 });
            drop(conn);
            let conn = crate::db::get_db().lock().unwrap();
            instances.set(provider_instance::list_all(&conn).unwrap_or_default());
            editing_key_for.set(None);
        }
    };

    // ── 添加实例（预设）──
    let mut do_add_preset = {
        move |preset: &&'static ProviderPreset| {
            let now = chrono::Local::now().to_rfc3339();
            let alias = if add_alias.read().trim().is_empty() {
                preset.name.to_string()
            } else {
                add_alias.read().trim().to_string()
            };
            let raw_key = add_key.read().trim().to_string();
            let encoded = encode_key(&raw_key);
            let row = ProviderInstanceRow {
                id: gen_id(),
                provider_id: preset.id.to_string(),
                alias,
                api_key: encoded,
                sort_order: 0,
                created_at: now,
            };
            let conn = crate::db::get_db().lock().unwrap();
            if let Err(e) = provider_instance::insert(&conn, &row) { tracing::error!(target:"db", error=%e, "insert provider instance."); }
            drop(conn);
            let conn = crate::db::get_db().lock().unwrap();
            instances.set(provider_instance::list_all(&conn).unwrap_or_default());
            on_change.call(());
            // 重置添加表单
            show_add_form.set(false);
            add_step.set(AddStep::SelectProvider);
            add_provider_id.set(String::new());
            add_alias.set(String::new());
            add_key.set(String::new());
        }
    };

    // ── 添加自定义 provider → 实例 ──
    let mut do_add_custom = {
        move |_| {
            let id = custom_id.read().trim().to_string();
            let name = custom_name.read().trim().to_string();
            let url = custom_url.read().trim().to_string();
            if id.is_empty() || name.is_empty() || url.is_empty() {
                return;
            }
            // 先写入 providers 表
            let conn = crate::db::get_db().lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO providers (id, name, kind, base_url, is_preset)
                 VALUES (?1, ?2, 'openai', ?3, 0)",
                rusqlite::params![id, name, url],
            )
            .unwrap_or_else(|e| { tracing::error!(target:"db", error=%e, "insert custom provider."); 0 });
            drop(conn);
            // 刷新 providers 缓存
            let conn = crate::db::get_db().lock().unwrap();
            providers_cache.set(provider::list_all(&conn).unwrap_or_default());
            drop(conn);
            // 创建实例
            let now = chrono::Local::now().to_rfc3339();
            let alias = if add_alias.read().trim().is_empty() {
                name
            } else {
                add_alias.read().trim().to_string()
            };
            let raw_key = add_key.read().trim().to_string();
            let encoded = encode_key(&raw_key);
            let row = ProviderInstanceRow {
                id: gen_id(),
                provider_id: id,
                alias,
                api_key: encoded,
                sort_order: 0,
                created_at: now,
            };
            let conn = crate::db::get_db().lock().unwrap();
            if let Err(e) = provider_instance::insert(&conn, &row) { tracing::error!(target:"db", error=%e, "insert provider instance."); }
            drop(conn);
            let conn = crate::db::get_db().lock().unwrap();
            instances.set(provider_instance::list_all(&conn).unwrap_or_default());
            on_change.call(());
            show_add_form.set(false);
            add_step.set(AddStep::SelectProvider);
            custom_id.set(String::new());
            custom_name.set(String::new());
            custom_url.set(String::new());
            add_alias.set(String::new());
            add_key.set(String::new());
        }
    };

    let presets = provider_presets::all_presets();

    // ── 辅助：找到 instance 对应的 provider 名称 ──
    let provider_name = |pid: &str| -> String {
        provs
            .iter()
            .find(|p| p.id == pid)
            .map(|p| p.name.clone())
            .unwrap_or_else(|| pid.to_string())
    };

    // ── Appearance 信号（match 外，hooks 顺序稳定） ──
    let mut current_font = use_signal(|| settings::get().appearance.font_size.clone());
    let mut current_code = use_signal(|| settings::get().appearance.code_font.clone());
    let mut current_density = use_signal(|| settings::get().appearance.ui_density.clone());
    let font_options = [
        ("xs", "xs (12px)"),
        ("sm", "sm (14px)"),
        ("md", "md (16px)"),
        ("lg", "lg (18px)"),
        ("xl", "xl (20px)"),
    ];
    let code_fonts = [
        ("jetbrains-mono", "JetBrains Mono"),
        ("geist-mono", "Geist Mono"),
        ("ibm-plex-mono", "IBM Plex Mono"),
        ("commit-mono", "Commit Mono"),
    ];
    let density_options = [
        ("comfortable", "comfortable"),
        ("compact", "compact"),
    ];

    // ── General 信号（match 外） ──
    let mut current_agent_id = use_signal(|| settings::get().general.default_agent_config_id.clone());
    let mut current_action_mode = use_signal(|| settings::get().general.default_action_mode.clone());
    let mut current_agent_mode = use_signal(|| settings::get().general.default_agent_mode.clone());

    rsx! {
        div { class: "settings-panel",
            match tab {
                SettingsTab::Providers => {
                    rsx! {
                        div { class: "settings-header",
                            h2 { class: "settings-title", "provider instances" }
                            span { class: "settings-subtitle", "manage LLM service connections" }
                        }
                        div { class: "settings-section",
                            // ── 添加按钮（顶部）──
                            if adding {
                                div {
                                    class: "settings-modal-backdrop",
                                    onclick: move |_| show_add_form.set(false),
                                    div {
                                        class: "settings-modal-panel",
                                        onclick: move |evt| evt.stop_propagation(),
                                        div { class: "settings-modal-header",
                                            span { class: "settings-modal-title", "add provider instance" }
                                            button {
                                                class: "settings-modal-close",
                                                onclick: move |_| { show_add_form.set(false); add_step.set(AddStep::SelectProvider); add_alias.set(String::new()); add_key.set(String::new()); custom_id.set(String::new()); custom_name.set(String::new()); custom_url.set(String::new()); },
                                                "✕"
                                            }
                                        }
                                        div { class: "settings-modal-body",
                    div { class: "provider-form",
                        if add_step() == AddStep::SelectProvider {
                            // 第一步：选择预设或自定义
                            div { class: "provider-form-tabs",
                                button {
                                    class: "provider-form-tab provider-form-tab--active",
                                    onclick: move |_| add_step.set(AddStep::SelectProvider),
                                    "select provider"
                                }
                                button {
                                    class: "provider-form-tab",
                                    onclick: move |_| add_step.set(AddStep::Custom),
                                    "custom"
                                }
                            }
                            div { class: "provider-preset-grid",
                                for preset in presets {
                                    {
                                        let p = preset;
                                        let is_used = ins.iter().any(|r| r.provider_id == p.id);
                                        rsx! {
                                            button {
                                                class: "provider-preset-btn",
                                                onclick: {
                                                    let pp = p;
                                                    move |_| {
                                                        add_provider_id.set(pp.id.to_string());
                                                        add_alias.set(pp.name.to_string());
                                                        add_step.set(AddStep::FillDetails);
                                                    }
                                                },
                                                span { class: "provider-preset-btn-name", "{p.name}" }
                                                span { class: "provider-preset-btn-url", "{p.base_url}" }
                                                if is_used {
                                                    span { class: "provider-preset-btn-check", "in use" }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "provider-form-separator", "or" }
                            button {
                                class: "provider-add-btn",
                                onclick: move |_| add_step.set(AddStep::Custom),
                                "+ custom provider"
                            }
                        } else if add_step() == AddStep::Custom {
                            // 自定义 provider
                            div { class: "provider-custom-form",
                                div { class: "settings-field",
                                    label { class: "settings-field-label", "provider id" }
                                    input { class: "settings-input", placeholder: "e.g. my-provider", value: "{custom_id}", oninput: move |evt| custom_id.set(evt.value()) }
                                }
                                div { class: "settings-field",
                                    label { class: "settings-field-label", "display name" }
                                    input { class: "settings-input", placeholder: "e.g. My Provider", value: "{custom_name}", oninput: move |evt| custom_name.set(evt.value()) }
                                }
                                div { class: "settings-field",
                                    label { class: "settings-field-label", "base url" }
                                    input { class: "settings-input", placeholder: "https://api.example.com/v1", value: "{custom_url}", oninput: move |evt| custom_url.set(evt.value()) }
                                }
                                div { class: "settings-field",
                                    label { class: "settings-field-label", "alias" }
                                    input { class: "settings-input", placeholder: "display name", value: "{add_alias}", oninput: move |evt| add_alias.set(evt.value()) }
                                }
                                div { class: "settings-field",
                                    label { class: "settings-field-label", "api key" }
                                    input { class: "settings-input", r#type: "password", placeholder: "sk-...", value: "{add_key}", oninput: move |evt| add_key.set(evt.value()) }
                                }
                                div { class: "provider-custom-form-actions",
                                    button { class: "btn btn-cancel", onclick: move |_| { show_add_form.set(false); add_step.set(AddStep::SelectProvider); add_alias.set(String::new()); add_key.set(String::new()); custom_id.set(String::new()); custom_name.set(String::new()); custom_url.set(String::new()); }, "cancel" }
                                    button { class: "btn btn-send", onclick: move |_| do_add_custom(()), "add instance" }
                                }
                            }
                        }

                        if add_step() == AddStep::FillDetails {
                            div { class: "provider-add-details",
                                div { class: "settings-field",
                                    label { class: "settings-field-label", "alias" }
                                    input {
                                        class: "settings-input",
                                        placeholder: "display name for this instance",
                                        value: "{add_alias}",
                                        oninput: move |evt| add_alias.set(evt.value()),
                                    }
                                }
                                div { class: "settings-field",
                                    label { class: "settings-field-label", "api key" }
                                    input {
                                        class: "settings-input",
                                        r#type: "password",
                                        placeholder: "sk-...",
                                        value: "{add_key}",
                                        oninput: move |evt| add_key.set(evt.value()),
                                    }
                                }
                                div { class: "provider-custom-form-actions",
                                    button {
                                        class: "btn btn-cancel",
                                        onclick: move |_| {
                                            show_add_form.set(false);
                                            add_step.set(AddStep::SelectProvider);
                                            add_alias.set(String::new());
                                            add_key.set(String::new());
                                        },
                                        "cancel"
                                    }
                                    button {
                                        class: "btn btn-send",
                                        onclick: move |_| {
                                            // 判断是 preset 还是 custom
                                            let pid = add_provider_id.read().clone();
                                            if pid.is_empty() {
                                                do_add_custom(());
                                            } else if let Some(preset) = presets.iter().find(|p| p.id == pid) {
                                                do_add_preset(&preset);
                                            }
                                        },
                                        "add instance"
                                    }
                                }
                            }
                        }
                            }
                        }
                    }
                    }
                } else {
                    button {
                        class: "provider-add-btn",
                        onclick: move |_| show_add_form.set(true),
                        "+ add provider instance"
                    }
                }

                // ── 实例列表 ──
                for inst in &ins {
                    {
                        let iid = inst.id.clone();
                        let pid = inst.provider_id.clone();
                        let pname = provider_name(&pid);
                        let has_key = !inst.api_key.is_empty();
                        let prov_models = models.get(&pid).cloned().unwrap_or_default();
                        let is_refreshing = refreshing() == Some(iid.clone());
                        let err = errs.get(&iid).cloned();
                        let show_confirm_delete = deleting() == Some(iid.clone());

                        rsx! {
                            div {
                                class: "provider-block",

                                // ── Header ──
                                div { class: "provider-block-header",
                                    div { class: "provider-block-title-row",
                                        div { class: "provider-block-info",
                                            div { class: "provider-block-name-row",
                                                span { class: "provider-block-name", "{inst.alias}" }
                                                span { class: "provider-badge-kind", "{pname}" }
                                            }
                                            span { class: "provider-block-url", "via {pid}" }
                                        }
                                    }
                                }

                                // ── Body ──
                                div { class: "provider-block-body",
                                    div { class: "provider-block-row",
                                        div { class: "provider-block-row-left",
                                            span { class: "provider-block-label", "api key" }
                                            if has_key {
                                                span { class: "provider-key-badge provider-key-badge--ok", "key set" }
                                            } else {
                                                span { class: "provider-key-badge", "no key" }
                                            }
                                        }
                                        button {
                                            class: "provider-block-action-btn",
                                            onclick: {
                                                let sid = iid.clone();
                                                let mut ek = editing_key_for;
                                                let mut ki = key_input;
                                                move |_| {
                                                    if ek() == Some(sid.clone()) { ek.set(None); }
                                                    else {
                                                        ki.set(String::new());
                                                        ek.set(Some(sid.clone()));
                                                    }
                                                }
                                            },
                                            "change key"
                                        }
                                    }
                                    if editing_key_for() == Some(iid.clone()) {
                                        div { class: "provider-key-edit",
                                            div { class: "settings-input-row",
                                                input {
                                                    class: "settings-input",
                                                    r#type: "password",
                                                    placeholder: "new api key",
                                                    oninput: move |evt| key_input.set(evt.value()),
                                                }
                                                button {
                                                    class: "btn btn-send",
                                                    style: "padding: 4px 12px; font-size: 11px;",
                                                    onclick: {
                                                        let sid = iid.clone();
                                                        move |_| do_save_key(sid.clone())
                                                    },
                                                    "save key"
                                                }
                                            }
                                        }
                                    }

                                    // 模型
                                    div { class: "provider-block-row provider-block-row--models",
                                        div { class: "provider-block-row-left",
                                            span { class: "provider-block-label", "models" }
                                        }
                                        button {
                                            class: "provider-block-action-btn",
                                            disabled: is_refreshing,
                                            onclick: {
                                                let sid = iid.clone();
                                                move |_| do_refresh(sid.clone())
                                            },
                                            if is_refreshing { "refreshing..." } else { "refresh" }
                                        }
                                    }
                                    if !prov_models.is_empty() {
                                        div { class: "model-pill-grid provider-block-models",
                                            for m in &prov_models {
                                                {
                                                    let model = m.clone();
                                                    rsx! {
                                                        span { class: "mode-pill", "{model}" }
                                                    }
                                                }
                                            }
                                        }
                                    } else if !is_refreshing {
                                        span { class: "provider-block-empty-models", "no models loaded — click refresh" }
                                    }
                                    if let Some(ref e) = err {
                                        div { class: "provider-block-error", "{e}" }
                                    }
                                }

                                // ── Footer ──
                                div { class: "provider-block-footer",
                                    if show_confirm_delete {
                                        div { class: "provider-block-confirm-delete",
                                            span { class: "provider-block-confirm-text",
                                                "delete \"{inst.alias}\"? this cannot be undone."
                                            }
                                            div { class: "provider-block-confirm-actions",
                                                button { class: "btn btn-cancel", style: "padding: 2px 10px; font-size: 10px;", onclick: move |_| deleting.set(None), "cancel" }
                                                button { class: "btn btn-send", style: "padding: 2px 10px; font-size: 10px; background: var(--color-error); color: var(--color-paper);", onclick: { let sid = iid.clone(); move |_| do_delete(sid.clone()) }, "confirm delete" }
                                            }
                                        }
                                    } else {
                                        button {
                                            class: "provider-block-delete-btn",
                                            onclick: { let sid = iid.clone(); move |_| deleting.set(Some(sid.clone())) },
                                            "delete instance"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // ── 空状态 ──
                if ins.is_empty() && !adding {
                    div { class: "provider-empty-state",
                        span { "no provider instances yet" }
                        span { style: "color: var(--color-ink-4); font-size: var(--text-sm);", "click \"+ add provider instance\" to connect your first LLM service" }
                    }
                }
                    }
                }
            }
                SettingsTab::AgentConfigs => {
                    rsx! {
                        div { class: "settings-header",
                            h2 { class: "settings-title", "agent configs" }
                            span { class: "settings-subtitle", "manage agent profiles and tool permissions" }
                        }
                        div { class: "settings-section",
                            AgentConfigPanel { }
                        }
                    }
                }
                SettingsTab::General => {
                    let all_agent_configs = {
                        let conn = crate::db::get_db().lock().unwrap();
                        agent_config::list_all(&conn).unwrap_or_default()
                    };
                    // Build dropdown options
                    let mut default_agent_opts: Vec<DropdownOption> = if all_agent_configs.is_empty() {
                        vec![DropdownOption { value: String::new(), label: "— none —".into() }]
                    } else {
                        Vec::new()
                    };
                    for cfg in &all_agent_configs {
                        default_agent_opts.push(DropdownOption {
                            value: cfg.id.clone(),
                            label: cfg.name.clone(),
                        });
                    }
                    let action_mode_opts = vec![
                        DropdownOption { value: "regular".into(), label: "regular — all tools available".into() },
                        DropdownOption { value: "plan".into(), label: "plan — read-only tools only".into() },
                    ];
                    let agent_mode_opts = vec![
                        DropdownOption { value: "cautious".into(), label: "cautious — all writes require approval".into() },
                        DropdownOption { value: "ask".into(), label: "ask — prompt on non-read operations".into() },
                        DropdownOption { value: "auto".into(), label: "auto — automatic approval for safe operations".into() },
                        DropdownOption { value: "unleashed".into(), label: "unleashed — full autonomy".into() },
                    ];

                    rsx! {
                        div { class: "settings-header",
                            h2 { class: "settings-title", "general" }
                            span { class: "settings-subtitle", "default preferences for new sessions" }
                        }
                        div { class: "settings-section",
                            div { class: "settings-field",
                                label { class: "settings-field-label", "default agent" }
                                Dropdown {
                                    value: current_agent_id(),
                                    options: default_agent_opts,
                                    onchange: move |val: String| {
                                        current_agent_id.set(val.clone());
                                        settings::update(|s| s.general.default_agent_config_id = val);
                                    },
                                }
                            }
                            div { class: "settings-field",
                                label { class: "settings-field-label", "default action mode" }
                                Dropdown {
                                    value: current_action_mode(),
                                    options: action_mode_opts,
                                    onchange: move |val: String| {
                                        current_action_mode.set(val.clone());
                                        settings::update(|s| s.general.default_action_mode = val);
                                    },
                                }
                            }
                            div { class: "settings-field",
                                label { class: "settings-field-label", "default agent mode" }
                                Dropdown {
                                    value: current_agent_mode(),
                                    options: agent_mode_opts,
                                    onchange: move |val: String| {
                                        current_agent_mode.set(val.clone());
                                        settings::update(|s| s.general.default_agent_mode = val);
                                    },
                                }
                            }
                        }
                    }
                }
                SettingsTab::Appearance => {
                    rsx! {
                        div { class: "settings-header",
                            h2 { class: "settings-title", "appearance" }
                            span { class: "settings-subtitle", "theme, typography, and density" }
                        }
                        div { class: "settings-section",
                            div { class: "settings-field",
                                label { class: "settings-field-label", "font size" }
                                div { class: "mode-pill-row",
                                    for (key, label) in font_options {
                                        button {
                                            class: if current_font.read().as_str() == key { "mode-pill is-active" } else { "mode-pill" },
                                            onclick: {
                                                let k = key.to_string();
                                                move |_| {
                                                    settings::update(|s| s.appearance.font_size = k.clone());
                                                    current_font.set(k.clone());
                                                }
                                            },
                                            "{label}"
                                        }
                                    }
                                }
                            }
                            div { class: "settings-field",
                                label { class: "settings-field-label", "code font" }
                                Dropdown {
                                    value: current_code.read().clone(),
                                    options: code_fonts.iter().map(|(key, label)| DropdownOption {
                                        value: key.to_string(),
                                        label: label.to_string(),
                                    }).collect(),
                                    onchange: move |val: String| {
                                        settings::update(|s| s.appearance.code_font = val.clone());
                                        current_code.set(val);
                                    },
                                }
                            }
                            div { class: "settings-field",
                                label { class: "settings-field-label", "ui density" }
                                div { class: "mode-pill-row",
                                    for (key, label) in density_options {
                                        button {
                                            class: if current_density.read().as_str() == key { "mode-pill is-active" } else { "mode-pill" },
                                            onclick: {
                                                move |_| {
                                                    let k = key.to_string();
                                                    settings::update(|s| s.appearance.ui_density = k.clone());
                                                    current_density.set(k);
                                                }
                                            },
                                            "{label}"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                SettingsTab::Sql => {
                    rsx! {
                        SqlPanel {}
                    }
                }
            }
        }
    }
}

#[derive(Clone, PartialEq)]
enum AddStep {
    SelectProvider,
    Custom,
    FillDetails,
}
