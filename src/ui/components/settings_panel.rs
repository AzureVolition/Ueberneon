// ── Provider 实例配置面板 ──
//
// 只显示已接入的 provider 实例，每个实例块可删除 / 刷新模型 / 更改密钥。
// 添加实例时从 providers 表（预设）选择服务，再指定 alias + key。

use dioxus::prelude::*;

use crate::db::metadata::agent_config::{self, AgentType};
use crate::db::metadata::provider::{self, ProviderRow};
use crate::db::metadata::provider_instance::{self, ProviderInstanceRow};
use crate::db::provider_presets::{self, ProviderPreset};
use crate::settings;
use crate::ui::components::agent_config_panel::AgentConfigPanel;
use crate::ui::components::dropdown::{Dropdown, DropdownOption};
use crate::ui::components::sql_panel::SqlPanel;
use crate::ui::components::tools_panel::ToolsPanel;
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
        crate::db::with_db(|conn| provider_instance::list_all(conn).unwrap_or_default())
    });
    let providers_cache: Signal<Vec<ProviderRow>> = use_signal(|| {
        crate::db::with_db(|conn| provider::list_all(conn).unwrap_or_default())
    });
    let mut models_cache: Signal<std::collections::HashMap<String, Vec<String>>> =
        use_signal(|| {
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
    let mut add_instance_error = use_signal(|| Option::<String>::None);

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
            let inst = crate::db::with_db(|conn| {
                provider_instance::get(conn, &inst_id).ok().flatten()
            });
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
                let prov = crate::db::with_db(|conn| {
                    provider::get(conn, &pid2).ok().flatten()
                });
                match prov {
                    Some(p) => {
                        match crate::db::model_fetch::refresh_and_save(
                            &crate::db::get_db().lock().expect("db lock poisoned"),
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
            crate::db::with_db(|conn| {
                if let Err(e) = provider_instance::delete(conn, &inst_id) { tracing::error!(target:"db", error=%e, "delete provider instance"); }
            });
            instances.set(crate::db::with_db(|conn| {
                provider_instance::list_all(conn).unwrap_or_default()
            }));
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
            crate::db::with_db(|conn| {
                if let Err(e) = provider_instance::update_key(conn, &inst_id, &encoded) { tracing::error!(target:"db", error=%e, "update provider key"); }
                conn.execute(
                    "UPDATE agent_configs SET api_key = ?1, updated_at = ?2 WHERE provider_instance_id = ?3",
                    rusqlite::params![encoded, chrono::Local::now().to_rfc3339(), inst_id],
                ).unwrap_or_else(|e| { tracing::error!(target:"db", error=%e, "update agent configs key."); 0 });
            });
            instances.set(crate::db::with_db(|conn| {
                provider_instance::list_all(conn).unwrap_or_default()
            }));
            editing_key_for.set(None);
        }
    };

    // ── 添加实例（预设）──
    let mut do_add_preset = {
        let mut err_sig = add_instance_error;
        let mut instances_sig = instances;
        let mut refresh_sig = refreshing;
        let mut models_sig = models_cache;
        let oc = on_change;
        move |preset: &&'static ProviderPreset| {
            let now = chrono::Local::now().to_rfc3339();
            let alias = if add_alias.read().trim().is_empty() {
                preset.name.to_string()
            } else {
                add_alias.read().trim().to_string()
            };
            let raw_key = add_key.read().trim().to_string();
            if raw_key.is_empty() {
                err_sig.set(Some("api key is required".into()));
                return;
            }
            // 检查 alias 是否重复（直接查 DB，确保一致性）
            let alias_dup = crate::db::with_db(|conn| {
                provider_instance::list_all(conn)
                    .map(|list| list.iter().any(|i| i.alias.eq_ignore_ascii_case(&alias)))
                    .unwrap_or(false)
            });
            if alias_dup {
                err_sig.set(Some(format!("alias \"{alias}\" is already in use")));
                return;
            }
            let encoded = encode_key(&raw_key);
            let ins_id = gen_id();
            let prov_row = ProviderRow {
                id: preset.id.to_string(),
                name: preset.name.to_string(),
                kind: preset.kind.to_string(),
                base_url: preset.base_url.to_string(),
                models_url: preset.models_url.to_string(),
                balance_url: String::new(),
                context_window: preset.context_window,
                is_preset: true,
            };
            let preset_id = preset.id.to_string();
            refresh_sig.set(Some(ins_id.clone()));
            err_sig.set(None);
            spawn(async move {
                // 只做 HTTP 请求，不持有 DB 锁
                match crate::db::model_fetch::fetch_models(&prov_row, &raw_key).await {
                    Err(e) => {
                        err_sig.set(Some(format!("key validation failed: {e}")));
                        refresh_sig.set(None);
                    }
                    Ok(models) => {
                        // 写入 DB（短暂持锁）
                        let row = ProviderInstanceRow {
                            id: ins_id.clone(),
                            provider_id: preset_id,
                            alias,
                            api_key: encoded,
                            sort_order: 0,
                            created_at: now,
                        };
                        let write_result = crate::db::with_db_result(|conn| {
                            provider::replace_models(conn, &prov_row.id, &models)
                                .map_err(|e| format!("db: {e}"))?;
                            provider_instance::insert(conn, &row)
                                .map_err(|e| format!("db: {e}"))
                        });
                        match write_result {
                            Err(e) => {
                                err_sig.set(Some(format!("failed to save: {e}")));
                            }
                            Ok(()) => {
                                models_sig.write().insert(prov_row.id, models);
                                match crate::db::with_db(|conn| provider_instance::list_all(conn)) {
                                    Ok(list) => instances_sig.set(list),
                                    Err(e) => {
                                        err_sig.set(Some(format!("saved but failed to refresh list: {e}")));
                                        instances_sig.set(Vec::new());
                                    }
                                }
                                oc.call(());
                                show_add_form.set(false);
                                add_step.set(AddStep::SelectProvider);
                                add_provider_id.set(String::new());
                                add_alias.set(String::new());
                                add_key.set(String::new());
                            }
                        }
                        refresh_sig.set(None);
                    }
                }
            });
        }
    };

    // ── 添加自定义 provider → 实例 ──
    let mut do_add_custom = {
        let mut err_sig = add_instance_error;
        let mut instances_sig = instances;
        let mut providers_cache_sig = providers_cache;
        let mut refresh_sig = refreshing;
        let oc = on_change;
        move |_| {
            let id = custom_id.read().trim().to_string();
            let name = custom_name.read().trim().to_string();
            let url = custom_url.read().trim().to_string();
            if id.is_empty() || name.is_empty() || url.is_empty() {
                return;
            }
            let raw_key = add_key.read().trim().to_string();
            if raw_key.is_empty() {
                err_sig.set(Some("api key is required".into()));
                return;
            }
            let alias = if add_alias.read().trim().is_empty() {
                name.clone()
            } else {
                add_alias.read().trim().to_string()
            };
            // 检查 alias 是否重复（直接查 DB，确保一致性）
            let alias_dup = crate::db::with_db(|conn| {
                provider_instance::list_all(conn)
                    .map(|list| list.iter().any(|i| i.alias.eq_ignore_ascii_case(&alias)))
                    .unwrap_or(false)
            });
            if alias_dup {
                err_sig.set(Some(format!("alias \"{alias}\" is already in use")));
                return;
            }
            let encoded = encode_key(&raw_key);
            let ins_id = gen_id();
            let now = chrono::Local::now().to_rfc3339();
            let prov_row = ProviderRow {
                id: id.clone(),
                name: name.clone(),
                kind: "openai".to_string(),
                base_url: url.clone(),
                models_url: format!("{url}/models"),
                balance_url: String::new(),
                context_window: 0,
                is_preset: false,
            };
            err_sig.set(None);
            refresh_sig.set(Some(ins_id.clone()));
            spawn(async move {
                // 只做 HTTP 请求，不持有 DB 锁
                match crate::db::model_fetch::fetch_models(&prov_row, &raw_key).await {
                    Err(e) => {
                        err_sig.set(Some(format!("key validation failed: {e}")));
                        refresh_sig.set(None);
                    }
                    Ok(models) => {
                        // 写入 DB（短暂持锁）
                        let write_result = crate::db::with_db_result(|conn| {
                            conn.execute(
                                "INSERT OR IGNORE INTO providers (id, name, kind, base_url, is_preset)
                                 VALUES (?1, ?2, 'openai', ?3, 0)",
                                rusqlite::params![id, name, url],
                            ).map_err(|e| format!("db: {e}"))?;
                            provider::replace_models(conn, &prov_row.id, &models)
                                .map_err(|e| format!("db: {e}"))?;
                            let row = ProviderInstanceRow {
                                id: ins_id.clone(),
                                provider_id: prov_row.id,
                                alias,
                                api_key: encoded,
                                sort_order: 0,
                                created_at: now,
                            };
                            provider_instance::insert(conn, &row)
                                .map_err(|e| format!("db: {e}"))
                        });
                        match write_result {
                            Err(e) => {
                                err_sig.set(Some(format!("failed to save: {e}")));
                            }
                            Ok(()) => {
                                providers_cache_sig.set(
                                    crate::db::with_db(|conn| provider::list_all(conn).unwrap_or_default())
                                );
                                match crate::db::with_db(|conn| provider_instance::list_all(conn)) {
                                    Ok(list) => instances_sig.set(list),
                                    Err(e) => {
                                        err_sig.set(Some(format!("saved but failed to refresh list: {e}")));
                                        instances_sig.set(Vec::new());
                                    }
                                }
                                oc.call(());
                                show_add_form.set(false);
                                add_step.set(AddStep::SelectProvider);
                                custom_id.set(String::new());
                                custom_name.set(String::new());
                                custom_url.set(String::new());
                                add_alias.set(String::new());
                                add_key.set(String::new());
                            }
                        }
                        refresh_sig.set(None);
                    }
                }
            });
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
                                                onclick: move |_| { show_add_form.set(false); add_step.set(AddStep::SelectProvider); add_alias.set(String::new()); add_key.set(String::new()); custom_id.set(String::new()); custom_name.set(String::new()); custom_url.set(String::new()); add_instance_error.set(None); },
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
                                div { class: "form-feedback",
                                    if let Some(ref err) = add_instance_error() {
                                        div { class: "form-error", "{err}" }
                                    } else if refreshing().is_some() {
                                        div { class: "form-hint", "checking api key — this may take a moment" }
                                    }
                                }
                                div { class: "provider-custom-form-actions",
                                    button { class: "btn btn-cancel", onclick: move |_| { show_add_form.set(false); add_step.set(AddStep::SelectProvider); add_alias.set(String::new()); add_key.set(String::new()); custom_id.set(String::new()); custom_name.set(String::new()); custom_url.set(String::new()); add_instance_error.set(None); }, "cancel" }
                                    {
                                        let validating = refreshing().is_some();
                                        rsx! {
                                            button {
                                                class: if validating { "btn btn-send is-disabled" } else { "btn btn-send" },
                                                disabled: validating,
                                                onclick: move |_| do_add_custom(()),
                                                if validating { "validating key..." } else { "add instance" }
                                            }
                                        }
                                    }
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
                                div { class: "form-feedback",
                                    if let Some(ref err) = add_instance_error() {
                                        div { class: "form-error", "{err}" }
                                    } else if refreshing().is_some() {
                                        div { class: "form-hint", "checking api key — this may take a moment" }
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
                                            add_instance_error.set(None);
                                        },
                                        "cancel"
                                    }
                                    {
                                        let validating = refreshing().is_some();
                                        rsx! {
                                            button {
                                                class: if validating { "btn btn-send is-disabled" } else { "btn btn-send" },
                                                disabled: validating,
                                                onclick: move |_| {
                                                    let pid = add_provider_id.read().clone();
                                                    if pid.is_empty() {
                                                        do_add_custom(());
                                                    } else if let Some(preset) = presets.iter().find(|p| p.id == pid) {
                                                        do_add_preset(&preset);
                                                    }
                                                },
                                                if validating { "validating key..." } else { "add instance" }
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
                            span { class: "settings-subtitle", "manage custom agent profiles and tool permissions" }
                        }
                        div { class: "settings-section",
                            AgentConfigPanel { filter_agent_type: format!("{}", AgentType::Custom), readonly: false, edit_mode: "full".to_string(), on_change }
                        }
                    }
                }
                SettingsTab::SubAgents => {
                    // ── 默认 SubAgent 模型状态 ──
                    let instances: Vec<ProviderInstanceRow> = crate::db::with_db(|conn| {
                        provider_instance::list_all(conn).unwrap_or_default()
                    });
                    let providers_cache: Vec<ProviderRow> = crate::db::with_db(|conn| {
                        provider::list_all(conn).unwrap_or_default()
                    });
                    let models_cache: std::collections::HashMap<String, Vec<String>> = crate::db::with_db(|conn| {
                        let mut map = std::collections::HashMap::new();
                        if let Ok(providers) = provider::list_all(conn) {
                            for p in &providers {
                                if let Ok(models) = provider::list_models(conn, &p.id) {
                                    map.insert(p.id.clone(), models);
                                }
                            }
                        }
                        map
                    });
                    let provider_name_for_inst = |inst_id: &str| -> String {
                        let inst = instances.iter().find(|i| i.id == inst_id);
                        match inst {
                            Some(i) => providers_cache.iter().find(|p| p.id == i.provider_id)
                                .map(|p| p.name.clone()).unwrap_or_else(|| i.provider_id.clone()),
                            None => String::new(),
                        }
                    };
                    let provider_id_for_inst = |inst_id: &str| -> String {
                        instances.iter()
                            .find(|i| i.id == inst_id)
                            .map(|i| i.provider_id.clone())
                            .unwrap_or_default()
                    };
                    let mut default_subagent_inst = use_signal(|| settings::get().general.default_subagent_provider_instance_id.clone());
                    let default_subagent_model_val = settings::get().general.default_subagent_model.clone();
                    let mut default_subagent_model = use_signal(|| default_subagent_model_val);

                    rsx! {
                        div { class: "settings-header",
                            h2 { class: "settings-title", "sub agents" }
                            span { class: "settings-subtitle", "default model for sub-agents that have no model configured" }
                        }
                        div { class: "settings-section",
                            div { class: "settings-field",
                                label { class: "settings-field-label", "default provider instance" }
                                Dropdown {
                                    value: default_subagent_inst(),
                                    options: {
                                        let mut opts = vec![DropdownOption { value: String::new(), label: "— none —".into() }];
                                        for inst in &instances {
                                            let pn = provider_name_for_inst(&inst.id);
                                            opts.push(DropdownOption {
                                                value: inst.id.clone(),
                                                label: format!("{} ({})", inst.alias, pn),
                                            });
                                        }
                                        opts
                                    },
                                    onchange: move |val: String| {
                                        default_subagent_inst.set(val.clone());
                                        default_subagent_model.set(String::new());
                                        settings::update(|s| s.general.default_subagent_provider_instance_id = val);
                                    },
                                }
                            }
                            div { class: "settings-field",
                                label { class: "settings-field-label", "default model" }
                                Dropdown {
                                    value: default_subagent_model(),
                                    options: {
                                        let pid = provider_id_for_inst(&default_subagent_inst());
                                        let models = models_cache.get(&pid).cloned().unwrap_or_default();
                                        let mut opts = vec![DropdownOption { value: String::new(), label: "— select —".into() }];
                                        for m in &models {
                                            opts.push(DropdownOption { value: m.clone(), label: m.clone() });
                                        }
                                        opts
                                    },
                                    onchange: move |val: String| {
                                        default_subagent_model.set(val.clone());
                                        settings::update(|s| s.general.default_subagent_model = val);
                                    },
                                }
                            }
                        }
                        div { class: "settings-section",
                            AgentConfigPanel { filter_agent_type: "SubAgent".to_string(), readonly: true, edit_mode: "provider_only".to_string(), on_change }
                        }
                    }
                }
                SettingsTab::General => {
                    let all_agent_configs = crate::db::with_db(|conn| {
                        agent_config::list_all(conn).unwrap_or_default()
                    });
                    // Build dropdown options
                    let mut default_agent_opts: Vec<DropdownOption> = if all_agent_configs.is_empty() {
                        vec![DropdownOption { value: String::new(), label: "— none —".into() }]
                    } else {
                        Vec::new()
                    };
                    for cfg in &all_agent_configs {
                        if cfg.agent_type != "SubAgent" {
                            default_agent_opts.push(DropdownOption {
                                value: cfg.id.clone(),
                                label: cfg.name.clone(),
                            });
                        }
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
                                    onchange: {
                                        let oc = on_change;
                                        move |val: String| {
                                            current_agent_id.set(val.clone());
                                            settings::update(|s| s.general.default_agent_config_id = val);
                                            oc.call(());
                                        }
                                    },
                                }
                            }
                            div { class: "settings-field",
                                label { class: "settings-field-label", "default action mode" }
                                Dropdown {
                                    value: current_action_mode(),
                                    options: action_mode_opts,
                                    onchange: {
                                        let oc = on_change;
                                        move |val: String| {
                                            current_action_mode.set(val.clone());
                                            settings::update(|s| s.general.default_action_mode = val);
                                            oc.call(());
                                        }
                                    },
                                }
                            }
                            div { class: "settings-field",
                                label { class: "settings-field-label", "default agent mode" }
                                Dropdown {
                                    value: current_agent_mode(),
                                    options: agent_mode_opts,
                                    onchange: {
                                        let oc = on_change;
                                        move |val: String| {
                                            current_agent_mode.set(val.clone());
                                            settings::update(|s| s.general.default_agent_mode = val);
                                            oc.call(());
                                        }
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
                SettingsTab::Tools => {
                    rsx! {
                        ToolsPanel {}
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
