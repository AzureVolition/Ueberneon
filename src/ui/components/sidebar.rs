use dioxus::prelude::*;

use crate::ui::state::*;

/// 侧边栏 —— 项目列表 / 对话列表双视图
#[component]
pub fn Sidebar(
    projects: Signal<Vec<Project>>,
    active_project_id: Signal<Option<String>>,
    sidebar_view: Signal<SidebarView>,
    active_conversation_id: Signal<String>,
    on_new_project: EventHandler<(String, String)>,
    on_new_conversation: EventHandler<()>,
    on_select_project: EventHandler<String>,
    on_select_conversation: EventHandler<String>,
    on_back_to_projects: EventHandler<()>,
    on_delete_project: EventHandler<String>,
    on_delete_conversation: EventHandler<String>,
) -> Element {
    // ── 新建项目表单状态 ──
    let mut show_new_project_form = use_signal(|| false);
    let mut new_project_name = use_signal(String::new);
    let mut new_project_path = use_signal(String::new);

    // ── 右键菜单状态 ──
    let mut project_context_menu = use_signal(|| Option::<(f64, f64, String)>::None);
    let mut conv_context_menu = use_signal(|| Option::<(f64, f64, String)>::None);

    let view = sidebar_view.read().clone();

    // ── 项目右键菜单 ──
    let project_menu_overlay = {
        let guard = project_context_menu.read();
        let val = guard.as_ref().map(|(x, y, target_id)| {
            let is_default = *target_id == crate::ui::store::DEFAULT_PROJECT_ID;
            let tid = target_id.clone();
            let pos_x = *x;
            let pos_y = *y;
            rsx! {
                div {
                    class: "context-menu-overlay",
                    onclick: move |_| { project_context_menu.set(None); },
                    div {
                        class: "context-menu",
                        style: "left: {pos_x}px; top: {pos_y}px;",
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

                                    rsx! {
                                        div {
                                            key: "{proj_id}",
                                            class: if is_active {
                                                "project-item active"
                                            } else {
                                                "project-item"
                                            },
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
                                            let rounds = conversation_rounds(&conv.messages);
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
        }
    }
}
