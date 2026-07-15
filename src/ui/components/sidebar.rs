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
) -> Element {
    // ── 新建项目表单状态 ──
    let mut show_new_project_form = use_signal(|| false);
    let mut new_project_name = use_signal(String::new);
    let mut new_project_path = use_signal(String::new);

    let view = sidebar_view.read().clone();

    rsx! {
        div {
            class: "sidebar",

            match view {
                SidebarView::ProjectList => {
                    // ── 项目列表视图 ──
                    rsx! {
                        div {
                            class: "sidebar-header",
                            h3 { "racp agent" }
                            div {
                                class: "sidebar-header-row",
                                span { class: "sidebar-label", "01 · PROJECTS" }
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

                        // ── 新建项目内联表单 ──
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
                                                        // 从路径提取文件夹名作为项目名
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

                        // ── 项目列表 ──
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
                                                "conversation-item active"
                                            } else {
                                                "conversation-item"
                                            },
                                            onclick: move |_| {
                                                on_select_project.call(proj_id.clone());
                                            },
                                            span {
                                                class: "project-icon",
                                                "\u{1f4c1}"  /* folder icon */
                                            }
                                            span {
                                                class: "conversation-title",
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
                    // ── 对话列表视图 ──
                    let proj_name = projects.read().iter()
                        .find(|p| p.id == *project_id)
                        .map(|p| p.name.clone())
                        .unwrap_or_default();

                    rsx! {
                        div {
                            class: "sidebar-header",
                            div {
                                class: "sidebar-breadcrumb",
                                onclick: move |_| on_back_to_projects.call(()),
                                span { class: "breadcrumb-back", "\u{2190}" }
                                span { class: "breadcrumb-text", "{proj_name}" }
                            }
                            div {
                                class: "sidebar-header-row",
                                span { class: "sidebar-label", "01 · CONVERSATIONS" }
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
                                            let conv_title = conv.title.clone();
                                            let is_active = *active_conversation_id.read() == conv_id;

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
                                                            on_select_conversation.call(cid.clone());
                                                        }
                                                    },
                                                    span {
                                                        class: "conversation-title",
                                                        "{conv_title}"
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
