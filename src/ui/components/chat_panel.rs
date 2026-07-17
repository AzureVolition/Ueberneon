use dioxus::desktop::use_window;
use dioxus::prelude::*;
use std::sync::atomic::Ordering;

use crate::model::{ChatMessage, Role, StreamSegment, ToolCallRecord, ToolCallStatus, UiMessage};

/// 对话面板 —— 消息列表 + 流式输出 + 空状态 + 时序导航
#[component]
pub fn ChatPanel(
    messages: Signal<Vec<UiMessage>>,
    tick: Signal<u64>,
    is_streaming: Signal<bool>,
    markdown_to_html: fn(&str) -> String,
    on_approve: EventHandler<(bool,)>,
) -> Element {
    let msgs = messages.read();
    let _tick = tick(); // 绑定 tick，变化时触发重渲染
    let running = is_streaming();
    let window = use_window();
    let win = window.clone();
    let win2 = win.clone();

    // 收集用户消息索引
    let user_messages: Vec<(usize, String)> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| match m {
            UiMessage::Static(cm) => matches!(cm.role, Role::User),
            _ => false,
        })
        .map(|(i, m)| {
            let text = match m {
                UiMessage::Static(cm) => cm.content.trim().to_string(),
                _ => String::new(),
            };
            let preview: String = text.chars().take(60).collect();
            (i, if text.chars().count() > 60 { format!("{preview}…") } else { preview })
        })
        .collect();

    // 注入滚动监听
    use_effect(move || {
        let script = r#"
(function(){
var p=document.querySelector('.chat-panel');
if(!p||p._tl)return;
p._tl=1;
p._autoFollow=1;
p.addEventListener('scroll',function(){
var atBottom=p.scrollHeight-p.scrollTop-p.clientHeight<60;
p._autoFollow=atBottom?1:0;
},{passive:true});
function upd(){
var hits=document.querySelectorAll('.timeline-hit');
var best=null,bestD=1e9;
var r=p.getBoundingClientRect();
var cy=r.top+r.height/2;
hits.forEach(function(h){
var idx=h.getAttribute('data-index');
var msg=document.getElementById('msg-'+idx);
if(!msg)return;
var mr=msg.getBoundingClientRect();
var d=Math.abs(mr.top+mr.height/2-cy);
if(d<bestD){bestD=d;best=h;}
});
hits.forEach(function(h){h.classList.remove('active');});
if(best)best.classList.add('active');
}
p.addEventListener('scroll',upd,{passive:true});
window.addEventListener('resize',upd,{passive:true});
upd();
})();
(function(){
var p=document.querySelector('.chat-panel');if(!p)return;
var wasStreaming=false;
var ob=new MutationObserver(function(){
if(p._autoFollow!==0){
requestAnimationFrame(function(){
p.scrollTo({top:p.scrollHeight,behavior:'auto'});
});
}
var now=!!document.querySelector('.message-assistant.streaming');
if(!wasStreaming&&now){
var bubble=document.querySelector('.message-assistant.streaming');
var hit=document.querySelector('.timeline-hit.streaming');
if(bubble&&hit){
var anims=hit.getAnimations();
if(anims.length){
requestAnimationFrame(function(){
var t=anims[0].currentTime;
if(typeof t==='number')bubble.style.animationDelay=(-(t%3000))+'ms';
});
}
}
}
if(wasStreaming&&!now){
var msgs=document.querySelectorAll('.message-assistant:not(.streaming)');
var last=msgs[msgs.length-1];
if(last&&!last.dataset.cd){
last.dataset.cd='1';
last.classList.add('cooldown-start');
requestAnimationFrame(function(){
requestAnimationFrame(function(){
last.classList.add('cooldown-end');
});
});
}
}
wasStreaming=now;
});
ob.observe(p,{childList:true,subtree:true,characterData:true});
})();
"#;
        let _ = win2.webview.evaluate_script(script);
    });

    let streaming_key = "streaming-bubble";
    let awaiting_response = msgs.last().map(|m| matches!(m, UiMessage::Static(cm) if matches!(cm.role, Role::User))).unwrap_or(false);

    // 检查是否有正在等待审批的 tool call
    let has_approval_pending = msgs.iter().any(|m| {
        matches!(m, UiMessage::Streaming { tool_calls, .. }
            if tool_calls.lock().unwrap().iter().any(|tc| matches!(tc.status, ToolCallStatus::AwaitingApproval { .. })))
    });
    let last_user_idx = user_messages.last().map(|(i, _)| *i);

    rsx! {
        div {
            class: "chat-panel",

            if msgs.is_empty() {
                div {
                    class: "chat-empty",
                    span { class: "empty-eyebrow", "CHAT" }
                    h2 { dangerous_inner_html: "ready to <em>think</em> with you." }
                    p { "start a conversation — type your message below." }
                }
            }

            {msgs.iter().enumerate().map(|(i, msg)| {
                match msg {
                    UiMessage::Static(chat_msg) => {
                        let formatted_time = chat_msg.timestamp.format("%H:%M:%S").to_string();
                        let (role_label, role_class) = match chat_msg.role {
                            Role::User => ("USER", "user-role"),
                            Role::Assistant => ("ASSISTANT", ""),
                            Role::System => ("SYSTEM", ""),
                        };
                        let bubble_class = match chat_msg.role {
                            Role::User => "message-bubble message-user",
                            Role::Assistant => "message-bubble message-assistant",
                            Role::System => "message-bubble message-system",
                        };
                        let msg_id = format!("msg-{i}");
                        let segments = chat_msg.segments.clone();
                        let tool_calls = chat_msg.tool_calls.clone();
                        let content = chat_msg.content.clone();
                        rsx! {
                            div { key: "{i}", id: "{msg_id}", class: bubble_class,
                                div { class: "message-header",
                                    span { class: "message-role {role_class}", "{role_label}" }
                                    span { class: "message-time", "{formatted_time}" }
                                }
                                {
                                    if segments.is_empty() && !content.is_empty() {
                                        rsx! { div { class: "message-content", dangerous_inner_html: markdown_to_html(&content) } }
                                    } else {
                                        rsx! { {render_segments(false, &segments, &tool_calls, markdown_to_html, on_approve).into_iter()} }
                                    }
                                }
                            }
                        }
                    }
                    UiMessage::Streaming { segments, tool_calls, approval_tx, .. } => {
                        let segs = segments.lock().unwrap().clone();
                        let tcs = tool_calls.lock().unwrap().clone();
                        let has_approval = tcs.iter().any(|tc| matches!(tc.status, ToolCallStatus::AwaitingApproval { .. }));
                        let streaming_class = if has_approval {
                            "message-bubble message-assistant streaming awaiting-approval"
                        } else {
                            "message-bubble message-assistant streaming"
                        };
                        // 审批处理
                        let atx = approval_tx.clone();
                        let on_stream_approve = move |(allowed,): (bool,)| {
                            if let Some(tx) = atx.lock().unwrap().take() {
                                let _ = tx.send(allowed);
                            }
                        };
                        rsx! {
                            div { key: "{streaming_key}", class: "{streaming_class}",
                                {render_segments(true, &segs, &tcs, markdown_to_html, EventHandler::new(on_stream_approve)).into_iter()}
                            }
                        }
                    }
                }
            })}

            if running && awaiting_response && !msgs.iter().any(|m| matches!(m, UiMessage::Streaming { .. })) {
                div { class: "message-bubble message-assistant thinking",
                    div { class: "thinking-dots",
                        span { "." } span { "." } span { "." }
                    }
                }
            }

            if !user_messages.is_empty() {
                div { class: "chat-timeline",
                    {user_messages.iter().map(|(idx, preview)| {
                        let i = *idx;
                        let tooltip = preview.clone();
                        let w = win.clone();
                        let timeline_class = if has_approval_pending && Some(*idx) == last_user_idx {
                            "timeline-hit streaming awaiting-approval"
                        } else {
                            "timeline-hit"
                        };
                        rsx! {
                            div { class: "{timeline_class}", "data-index": "{idx}", title: "{tooltip}",
                                onclick: move |_| {{
                                    let script = format!(
                                        "(function(){{var el=document.getElementById('msg-{i}');if(!el)return;var p=el.closest('.chat-panel');if(!p){{el.scrollIntoView({{behavior:'smooth',block:'center'}});return;}}p.scrollTop=el.offsetTop-p.offsetHeight/2;}})()",
                                    );
                                    let _ = w.webview.evaluate_script(&script);
                                }},
                            }
                        }
                    })}
                }
            }
        }
    }
}

fn render_segments(
    streaming: bool,
    segments: &[StreamSegment],
    tool_calls: &[ToolCallRecord],
    markdown_to_html: fn(&str) -> String,
    on_approve: EventHandler<(bool,)>,
) -> Vec<Element> {
    let mut items: Vec<Element> = Vec::new();
    let mut tc_idx = 0usize;
    let mut buf: Vec<Element> = Vec::new();

    let mut flush = |buf: &mut Vec<Element>, items: &mut Vec<Element>| {
        if !buf.is_empty() {
            let children: Vec<Element> = buf.drain(..).collect();
            items.push(rsx! {
                details { class: "think-watch", open: streaming,                    summary { class: "think-watch-toggle", "think watch write" }
                    {children.into_iter()}
                }
            });
        }
    };

    for seg in segments {
        match seg {
            StreamSegment::Text(t) => {
                flush(&mut buf, &mut items);
                items.push(rsx! { div { class: "message-content", dangerous_inner_html: markdown_to_html(t) } });
            }
            StreamSegment::Reasoning(text) => {
                let html = markdown_to_html(text);
                buf.push(rsx! { div { class: "thinking-content", dangerous_inner_html: html } });
            }
            StreamSegment::ToolCall => {
                if let Some(call) = tool_calls.get(tc_idx) {
                    let sc = status_class(&call.status);
                    let status_text = match &call.status {
                        ToolCallStatus::Running => "running",
                        ToolCallStatus::Success => "success",
                        ToolCallStatus::Failed(_) => "failed",
                        ToolCallStatus::Denied(_) => "denied",
                        ToolCallStatus::AwaitingApproval { .. } => "needs approval",
                    };
                    let is_approval = matches!(&call.status, ToolCallStatus::AwaitingApproval { .. });
                    let approval_reason = call.approval_reason.clone().unwrap_or_default();
                    let tool_name = call.tool_name.clone();
                    let args_summary = tool_args_summary(&call.tool_name, &call.args);
                    let on_allow = on_approve;
                    let on_deny = on_approve;

                    buf.push(if is_approval {
                        rsx! {
                            div { class: "approval-card",
                                div { class: "approval-header", span { class: "approval-title", "{tool_name} needs approval" } }
                                div { class: "approval-body",
                                    div { class: "approval-args", "{args_summary}" }
                                    div { class: "approval-reason", "{approval_reason}" }
                                }
                                div { class: "approval-actions",
                                    button { class: "approval-btn allow", onclick: move |_| on_allow.call((true,)), "allow" }
                                    button { class: "approval-btn deny", onclick: move |_| on_deny.call((false,)), "deny" }
                                }
                            }
                        }
                    } else {
                        rsx! {
                            details { class: "tool-call-details {sc}",
                                summary { class: "tool-call-summary",
                                    span { class: "tool-call-name", "{call.tool_name}" }
                                    span { class: "tool-call-args", "{tool_args_summary(&call.tool_name, &call.args)}" }
                                    span { class: "tool-call-status {sc}", "{status_text}" }
                                }
                                if let Some(ref result) = call.result {
                                    pre { class: "tool-call-result", "{result}" }
                                }
                            }
                        }
                    });
                }
                tc_idx += 1;
            }
        }
    }
    flush(&mut buf, &mut items);
    items
}

fn status_class(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Running => "status-running",
        ToolCallStatus::Success => "status-success",
        ToolCallStatus::Failed(_) => "status-failed",
        ToolCallStatus::Denied(_) => "status-failed",
        ToolCallStatus::AwaitingApproval { .. } => "status-approval",
    }
}

fn tool_args_summary(tool_name: &str, args: &serde_json::Value) -> String {
    let keys: &[&str] = match tool_name {
        "bash" | "read_only_bash" => &["command"],
        "read_file" | "write_file" | "edit_file" | "multi_edit" => &["path"],
        "grep" => &["pattern", "path"],
        "glob" | "code_index" => &["pattern"],
        "web_fetch" => &["url"],
        "ls" => &["path"],
        _ => &["path", "command", "pattern", "url", "name"],
    };
    for key in keys {
        if let Some(val) = args.get(key) {
            if let Some(s) = val.as_str() { if !s.is_empty() { return s.to_string(); } }
        }
    }
    let json = serde_json::to_string(args).unwrap_or_default();
    if json.len() <= 60 { json } else { format!("{}…", &json[..57]) }
}
