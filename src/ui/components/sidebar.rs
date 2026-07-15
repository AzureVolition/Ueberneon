use dioxus::prelude::*;

use crate::ui::state::Conversation;

/// 侧边栏 —— 对话列表 + 新建按钮
#[component]
pub fn Sidebar(
    conversations: Signal<Vec<Conversation>>,
    active_conversation_id: Signal<String>,
    on_new_conversation: EventHandler<()>,
    on_select_conversation: EventHandler<String>,
) -> Element {
    let convs = conversations.read();

    rsx! {
        div {
            class: "sidebar",

            div {
                class: "sidebar-header",
                h3 { "racp agent" }
                span { class: "sidebar-label", "01 · CONVERSATIONS" }
                button {
                    class: "btn btn-new-chat",
                    onclick: move |_| on_new_conversation.call(()),
                    "+ NEW"
                }
            }

            div {
                class: "conversation-list",
                for conv in convs.iter() {
                    {
                        let conv_id = conv.id.clone();
                        let is_active = *active_conversation_id.read() == conv_id;
                        let title = conv.title.clone();

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
                                    "{title}"
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
