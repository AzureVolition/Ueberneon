use dioxus::prelude::*;

use crate::ui::state::*;
use crate::ui::state::SettingsTab;

/// 侧边栏 —— 项目列表 / 对话列表双视图
#[component]
pub fn Sidebar(
    projects: Signal<Vec<Project>>,
    active_project_id: Signal<Option<String>>,
    sidebar_view: Signal<SidebarView>,
    active_conversation_id: Signal<String>,
    streaming_project_id: Signal<Option<String>>,
    on_new_project: EventHandler<(String, String)>,
    on_new_conversation: EventHandler<()>,
    on_select_project: EventHandler<String>,
    on_select_conversation: EventHandler<String>,
    on_back_to_projects: EventHandler<()>,
    on_delete_project: EventHandler<String>,
    on_delete_conversation: EventHandler<String>,
    on_change_indicator_color: EventHandler<(String, String)>,
) -> Element {
    // ── 新建项目表单状态 ──
    let mut show_new_project_form = use_signal(|| false);
    let mut new_project_name = use_signal(String::new);
    let mut new_project_path = use_signal(String::new);

    // ── 右键菜单状态 ──
    let mut project_context_menu = use_signal(|| Option::<(f64, f64, String)>::None);
    let mut conv_context_menu = use_signal(|| Option::<(f64, f64, String)>::None);

    let view = sidebar_view.read().clone();

    let COLOR_SWATCHES: [(&str, &str, &str); 7] = [
        ("cyan",   "oklch(72% 0.20 200)",   "cyan"),
        ("pink",   "oklch(68% 0.22 330)",   "pink"),
        ("green",  "oklch(66% 0.18 145)",   "green"),
        ("orange", "oklch(70% 0.18 50)",    "orange"),
        ("violet", "oklch(65% 0.16 280)",   "violet"),
        ("blue",   "oklch(60% 0.20 240)",   "blue"),
        ("gold",   "oklch(72% 0.16 85)",    "gold"),
    ];

    // ── 项目右键菜单 ──
    let project_menu_overlay = {
        let guard = project_context_menu.read();
        let val = guard.as_ref().map(|(x, y, target_id)| {
            let is_default = *target_id == crate::db::DEFAULT_PROJECT_ID;
            let tid = target_id.clone();
            let pos_x = *x;
            let pos_y = *y;
            // 当前项目的颜色键
            let current_color: String = projects.read().iter()
                .find(|p| p.id == *target_id)
                .map(|p| if p.indicator_color.is_empty() { "cyan".into() } else { p.indicator_color.clone() })
                .unwrap_or_else(|| "cyan".into());
            let color_cloned = current_color.clone();
            rsx! {
                div {
                    class: "context-menu-overlay",
                    onclick: move |_| { project_context_menu.set(None); },
                    div {
                        class: "context-menu",
                        style: "left: {pos_x}px; top: {pos_y}px;",
                        // ── 颜色选择器 ──
                        div {
                            class: "context-menu-label",
                            "indicator color"
                        }
                        div {
                            class: "context-menu-color-grid",
                            for (key, color_val, _label) in COLOR_SWATCHES {
                                {
                                    let is_sel = color_cloned == key;
                                    let skey = key;
                                    let stid = tid.clone();
                                    rsx! {
                                        div {
                                            class: if is_sel { "context-menu-color-swatch selected" } else { "context-menu-color-swatch" },
                                            style: "background: {color_val};",
                                            onclick: move |_| {
                                                project_context_menu.set(None);
                                                on_change_indicator_color.call((stid.clone(), skey.to_string()));
                                            },
                                        }
                                    }
                                }
                            }
                        }
                        div { class: "context-menu-divider" }
                        if !is_default {
                            div {
                                class: "context-menu-item danger",
                                onclick: move |_| {
                                    on_delete_project.call(tid.clone());
                                    project_context_menu.set(None);
                                },
                                "delete project"
                            }
                        } else {
                            div {
                                class: "context-menu-item disabled",
                                "cannot delete default"
                            }
                        }
                    }
                }
            }
        });
        val
    };

    // ── 对话右键菜单 ──
    let conv_menu_overlay = {
        let guard = conv_context_menu.read();
        let val = guard.as_ref().map(|(x, y, target_id)| {
            let tid = target_id.clone();
            let pos_x = *x;
            let pos_y = *y;
            rsx! {
                div {
                    class: "context-menu-overlay",
                    onclick: move |_| { conv_context_menu.set(None); },
                    div {
                        class: "context-menu",
                        style: "left: {pos_x}px; top: {pos_y}px;",
                        div {
                            class: "context-menu-item danger",
                            onclick: move |_| {
                                on_delete_conversation.call(tid.clone());
                                conv_context_menu.set(None);
                            },
                            "delete conversation"
                        }
                    }
                }
            }
        });
        val
    };

    rsx! {
        div {
            class: "sidebar",

            {project_menu_overlay}
            {conv_menu_overlay}

            match view {
                SidebarView::Settings(ref current_tab) => {
                    let tabs = [
                        SettingsTab::Providers,
                        SettingsTab::AgentConfigs,
                        SettingsTab::SubAgents,
                        SettingsTab::General,
                        SettingsTab::Appearance,
                        SettingsTab::Tools,
                        SettingsTab::Sql,
                    ];
                    let ct = current_tab.clone();
                    rsx! {
                        div {
                            class: "sidebar-header",
                            div {
                                class: "sidebar-nav-back",
                                onclick: move |_| on_back_to_projects.call(()),
                                span { class: "sidebar-nav-label", "projects /" }
                                span { class: "sidebar-nav-project", "settings" }
                            }
                        }
                        div { class: "sidebar-nav-section",
                            for tab in tabs {
                                {
                                    let is_active = ct == tab;
                                    let t = tab.clone();
                                    rsx! {
                                        div {
                                            class: if is_active { "sidebar-nav-item active" } else { "sidebar-nav-item" },
                                            onclick: move |_| sidebar_view.set(SidebarView::Settings(t.clone())),
                                            span { class: "sidebar-nav-item-icon", "{t.icon()}" }
                                            span { "{t.label()}" }
                                            if is_active {
                                                span { class: "sidebar-nav-item-indicator" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                SidebarView::ProjectList => {
                    rsx! {
                        div {
                            class: "sidebar-header",
                            h3 { "racp agent" }
                            div {
                                class: "sidebar-header-row",
                                span { class: "sidebar-label", "PROJECTS" }
                                button {
                                    class: "btn btn-new-chat",
                                    onclick: move |_| {
                                        let mut show = show_new_project_form.write();
                                        *show = !*show;
                                        if *show {
                                            new_project_name.set(String::new());
                                            new_project_path.set(String::new());
                                        }
                                    },
                                    if *show_new_project_form.read() { "−" } else { "+ NEW" }
                                }
                            }
                        }

                        if *show_new_project_form.read() {
                            div {
                                class: "project-form",
                                div {
                                    class: "project-form-field",
                                    label { "name" }
                                    input {
                                        class: "project-form-input",
                                        value: "{new_project_name}",
                                        placeholder: "my project",
                                        oninput: move |evt| new_project_name.set(evt.value()),
                                    }
                                }
                                div {
                                    class: "project-form-field",
                                    label { "path" }
                                    div {
                                        class: "project-form-field-row",
                                        input {
                                            class: "project-form-input",
                                            value: "{new_project_path}",
                                            placeholder: "/path/to/project",
                                            oninput: move |evt| new_project_path.set(evt.value()),
                                        }
                                        button {
                                            class: "btn btn-browse",
                                            onclick: move |_| {
                                                let mut name_signal = new_project_name;
                                                let mut path_signal = new_project_path;
                                                spawn(async move {
                                                    if let Some(folder) = rfd::AsyncFileDialog::new()
                                                        .set_title("select project folder")
                                                        .pick_folder()
                                                        .await
                                                    {
                                                        let path = folder.path();
                                                        let path_str = path.display().to_string();
                                                        path_signal.set(path_str);
                                                        if let Some(folder_name) = path
                                                            .file_name()
                                                            .and_then(|n| n.to_str())
                                                        {
                                                            name_signal.set(folder_name.to_string());
                                                        }
                                                    }
                                                });
                                            },
                                            "browse"
                                        }
                                    }
                                }
                                div {
                                    class: "project-form-actions",
                                    button {
                                        class: "btn btn-cancel",
                                        onclick: move |_| {
                                            show_new_project_form.set(false);
                                            new_project_name.set(String::new());
                                        },
                                        "cancel"
                                    }
                                    button {
                                        class: "btn btn-send",
                                        onclick: move |_| {
                                            let name = new_project_name.read().trim().to_string();
                                            let path = new_project_path.read().trim().to_string();
                                            if !name.is_empty() && !path.is_empty() {
                                                on_new_project.call((name, path));
                                                show_new_project_form.set(false);
                                                new_project_name.set(String::new());
                                            }
                                        },
                                        "save"
                                    }
                                }
                            }
                        }

                        div {
                            class: "conversation-list",
                            for proj in projects.read().iter() {
                                {
                                    let proj_id = proj.id.clone();
                                    let proj_name = proj.name.clone();
                                    let is_active = active_project_id.read().as_deref() == Some(&proj_id);
                                    // 3 天内有活动 → recent
                                    let is_recent = proj.last_activity_at
                                        .map(|t| (chrono::Local::now() - t).num_hours() < 72)
                                        .unwrap_or(false);
                                    let data_color = if proj.indicator_color.is_empty() {
                                        "cyan".to_string()
                                    } else {
                                        proj.indicator_color.clone()
                                    };
                                    // 当前项目是否正在生成对话
                                    let is_streaming_this = *streaming_project_id.read() == Some(proj_id.clone());

                                    rsx! {
                                        div {
                                            key: "{proj_id}",
                                            class: if is_active {
                                                "project-item active"
                                            } else {
                                                "project-item"
                                            },
                                            "data-color": "{data_color}",
                                            "data-recent": if is_recent { "true" } else { "false" },
                                            "data-streaming": if is_streaming_this { "true" } else { "false" },
                                            onclick: {
                                                let pid = proj_id.clone();
                                                move |_| {
                                                    project_context_menu.set(None);
                                                    conv_context_menu.set(None);
                                                    on_select_project.call(pid.clone());
                                                }
                                            },
                                            oncontextmenu: {
                                                let pid = proj_id.clone();
                                                move |evt| {
                                                    evt.prevent_default();
                                                    let coords = evt.client_coordinates();
                                                    project_context_menu.set(Some((
                                                        coords.x,
                                                        coords.y,
                                                        pid.clone(),
                                                    )));
                                                }
                                            },
                                            span {
                                                class: "project-item-indicator",
                                            }
                                            span {
                                                class: "project-item-name",
                                                "{proj_name}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                SidebarView::ConversationList(ref project_id) => {
                    let proj_name = projects.read().iter()
                        .find(|p| p.id == *project_id)
                        .map(|p| p.name.clone())
                        .unwrap_or_default();

                    rsx! {
                        div {
                            class: "sidebar-header",
                            div {
                                class: "sidebar-nav-back",
                                onclick: move |_| on_back_to_projects.call(()),
                                span { class: "sidebar-nav-label", "projects /" }
                                span { class: "sidebar-nav-project", "{proj_name}" }
                            }
                            div {
                                class: "sidebar-header-row",
                                span { class: "sidebar-label", "CONVERSATIONS" }
                                button {
                                    class: "btn btn-new-chat",
                                    onclick: move |_| on_new_conversation.call(()),
                                    "+ NEW"
                                }
                            }
                        }

                        div {
                            class: "conversation-list",
                            {
                                let convs: Vec<Conversation> = projects.read().iter()
                                    .find(|p| p.id == *project_id)
                                    .map(|p| p.conversations.clone())
                                    .unwrap_or_default();

                                rsx! {
                                    for conv in convs.iter() {
                                        {
                                            let conv_id = conv.id.clone();
                                            let conv_title = if conv.title.is_empty() {
                                                "new conversation".into()
                                            } else {
                                                conv.title.clone()
                                            };
                                            let is_active = *active_conversation_id.read() == conv_id;
                                            let title_class = if conv.title.is_empty() {
                                                "conversation-title-placeholder"
                                            } else {
                                                "conversation-title"
                                            };
                                            let rounds = conv.message_count;
                                            let rounds_label = if rounds == 1 {
                                                "1 round".into()
                                            } else {
                                                format!("{rounds} rounds")
                                            };
                                            let time_str = format_relative_time(&conv.updated_at);

                                            rsx! {
                                                div {
                                                    key: "{conv_id}",
                                                    class: if is_active {
                                                        "conversation-item active"
                                                    } else {
                                                        "conversation-item"
                                                    },
                                                    onclick: {
                                                        let cid = conv_id.clone();
                                                        move |_| {
                                                            conv_context_menu.set(None);
                                                            on_select_conversation.call(cid.clone());
                                                        }
                                                    },
                                                    oncontextmenu: {
                                                        let cid = conv_id.clone();
                                                        move |evt| {
                                                            evt.prevent_default();
                                                            let coords = evt.client_coordinates();
                                                            conv_context_menu.set(Some((
                                                                coords.x,
                                                                coords.y,
                                                                cid.clone(),
                                                            )));
                                                        }
                                                    },
                                                    div {
                                                        class: "{title_class}",
                                                        "{conv_title}"
                                                    }
                                                    div {
                                                        class: "conversation-meta",
                                                        if rounds > 0 {
                                                            span {
                                                                class: "conv-rounds",
                                                                "{rounds_label}"
                                                            }
                                                        }
                                                        span {
                                                            class: "conv-time",
                                                            "{time_str}"
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

            // ── Settings footer (hidden when in settings view) ──
            if !matches!(sidebar_view(), SidebarView::Settings(_)) {
                div {
                    class: "sidebar-settings-row",
                    onclick: move |_| sidebar_view.set(SidebarView::Settings(SettingsTab::Providers)),
                    span {
                        class: "sidebar-settings-icon",
                        "⚙"
                    }
                    span {
                        class: "sidebar-settings-label",
                        "settings"
                    }
                }
            }
        }
    }
}
