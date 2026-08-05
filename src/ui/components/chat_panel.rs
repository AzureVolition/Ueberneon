use dioxus::desktop::use_window;
use dioxus::prelude::*;
use std::collections::HashMap;

use crate::model::{Role, StreamSegment, ToolCallStatus, UiMessage};
use crate::ui::state::ConversationRuntime;

/// 对话面板 —— 消息列表 + 流式输出 + 空状态 + 时序导航
#[component]
pub fn ChatPanel(
    runtimes: Signal<HashMap<String, ConversationRuntime>>,
    active_conv_id: Signal<String>,
    is_streaming: Signal<bool>,
    markdown_to_html: fn(&str) -> String,
    on_approve: EventHandler<(String, bool)>,
) -> Element {
    let cid = active_conv_id();
    let (msgs, _tick) = {
        let rt = runtimes.read();
        let r = rt.get(&cid);
        (
            r.map(|r| r.messages.clone()).unwrap_or_default(),
            r.map(|r| r.tick).unwrap_or(0),
        )
    };
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
            (
                i,
                if text.chars().count() > 60 {
                    format!("{preview}…")
                } else {
                    preview
                },
            )
        })
        .collect();

    // 注入滚动监听（仅首次挂载）
    let mut js_injected = use_signal(|| false);
    use_effect(move || {
        if *js_injected.read() { return; }
        js_injected.set(true);
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
    let awaiting_response = msgs
        .last()
        .map(|m| matches!(m, UiMessage::Static(cm) if matches!(cm.role, Role::User)))
        .unwrap_or(false);

    // 检查是否有正在等待审批的 tool call（遍历 segments 内嵌记录，单一数据源）
    let has_approval_pending = msgs.iter().any(|m| match m {
        UiMessage::Streaming { segments, .. } => segments
            .lock().expect("segments lock poisoned")
            .iter().any(|s| matches!(s, StreamSegment::ToolCall(tc) if matches!(tc.status, ToolCallStatus::AwaitingApproval { .. }))),
        UiMessage::Static(cm) => cm.segments.iter().any(|s| matches!(s, StreamSegment::ToolCall(tc) if matches!(tc.status, ToolCallStatus::AwaitingApproval { .. }))),
    });
    let expanded_tc = use_signal(|| std::collections::HashSet::<String>::new());
    let last_user_idx = user_messages.last().map(|(i, _)| *i);

    let el = rsx! {
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
                        rsx! {
                            div { key: "{i}", id: "{msg_id}", class: bubble_class,
                                div { class: "message-header",
                                    span { class: "message-role {role_class}", "{role_label}" }
                                    span { class: "message-time", "{formatted_time}" }
                                }
                                {
                                    if segments.is_empty() && !chat_msg.content.is_empty() {
                                        let html = if chat_msg.content_html.is_empty() {
                                            markdown_to_html(&chat_msg.content)
                                        } else {
                                            chat_msg.content_html.clone()
                                        };
                                        rsx! { div { class: "message-content", dangerous_inner_html: "{html}" } }
                                    } else {
                                        rsx! { {render_segments(false, &segments, markdown_to_html, on_approve, expanded_tc, format!("{i}")).into_iter()} }
                                    }
                                }
                            }
                        }
                    }
                    UiMessage::Streaming { segments, .. } => {
                        let segs = segments.lock().expect("segments lock poisoned").clone();
                        let has_approval = segs.iter().any(|s| matches!(s, StreamSegment::ToolCall(tc) if matches!(tc.status, ToolCallStatus::AwaitingApproval { .. })));
                        let streaming_class = if has_approval {
                            "message-bubble message-assistant streaming awaiting-approval"
                        } else {
                            "message-bubble message-assistant streaming"
                        };
                        rsx! {
                            div { key: "{streaming_key}", class: "{streaming_class}",
                                {render_segments(true, &segs, markdown_to_html, on_approve, expanded_tc, "stream".into()).into_iter()}
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
    };
    el
}

fn render_segments(
    streaming: bool,
    segments: &[StreamSegment],
    markdown_to_html: fn(&str) -> String,
    on_approve: EventHandler<(String, bool)>,
    expanded_tc: Signal<std::collections::HashSet<String>>,
    msg_key: String,
) -> Vec<Element> {
    let mut items: Vec<Element> = Vec::new();
    let mut tc_idx = 0usize;
    let mut buf: Vec<Element> = Vec::new();

    let flush = |buf: &mut Vec<Element>, items: &mut Vec<Element>| {
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
            StreamSegment::ToolCall(call) => {
                let sc = status_class(&call.status);
                    let status_text = match &call.status {
                        ToolCallStatus::Running => "running",
                        ToolCallStatus::Success => "success",
                        ToolCallStatus::Failed(_) => "failed",
                        ToolCallStatus::Denied(_) => "denied",
                        ToolCallStatus::AwaitingApproval { .. } => "needs approval",
                        ToolCallStatus::Pending => "pending",
                    };
                    let is_approval =
                        matches!(&call.status, ToolCallStatus::AwaitingApproval { .. });
                    let approval_reason = call.approval_reason.clone().unwrap_or_default();
                    let tool_name = call.tool_name.clone();
                    let args_summary = tool_args_summary(&call.tool_name, &call.args);
                    let call_id_allow = call.id.clone();
                    let call_id_deny = call.id.clone();
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
                                    button { class: "approval-btn allow", onclick: move |_| on_allow.call((call_id_allow.clone(), true)), "allow" }
                                    button { class: "approval-btn deny", onclick: move |_| on_deny.call((call_id_deny.clone(), false)), "deny" }
                                }
                            }
                        }
                    } else {
                        let tc_key = format!("{}:{}", msg_key, tc_idx);
                        let expanded = expanded_tc.read().contains(&tc_key);
                        let mut extc = expanded_tc;
                        let tck = tc_key.clone();
                        rsx! {
                            details { class: "tool-call-details {sc}",
                                summary {
                                    class: "tool-call-summary",
                                    onclick: move |_| {
                                        if extc.read().contains(&tck) {
                                            extc.write().remove(&tck);
                                        } else {
                                            extc.write().insert(tck.clone());
                                        }
                                    },
                                    span { class: "tool-call-name", "{call.tool_name}" }
                                    if call.tool_name == "write_file" || call.tool_name == "edit_file" {
                                        span { class: "tool-call-args", "{file_path_from_args(&call.args)}" }
                                        if let Some(ref result) = call.result {
                                            span { class: "tool-call-diff-stat", "{parse_diff_stat(result)}" }
                                        }
                                    } else {
                                        {
                                            let summary = tool_args_summary(&call.tool_name, &call.args);
                                            if !summary.is_empty() {
                                                rsx! { span { class: "tool-call-args", "{summary}" } }
                                            } else {
                                                rsx! {}
                                            }
                                        }
                                    }
                                    span { class: "tool-call-status {sc}", "{status_text}" }
                                }
                                if expanded {
                                    if call.tool_name == "WriteFile" || call.tool_name == "EditFile" {
                                        if let Some(ref result) = call.result {
                                            {render_diff_view(result)}
                                        }
                                    } else {
                                        {
                                            let mut md = format!("**Args**\n\n```json\n{}\n```",
                                                serde_json::to_string_pretty(&call.args).unwrap_or_default());
                                            if let Some(ref result) = call.result {
                                                md.push_str(&format!("\n\n**Result**\n\n```\n{}\n```", result));
                                            }
                                            rsx! {
                                                div { class: "tool-call-section",
                                                    div {
                                                        class: "tool-call-section-body",
                                                        dangerous_inner_html: markdown_to_html(&md),
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                });
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
        ToolCallStatus::Pending => "status-pending",
    }
}

fn tool_args_summary(tool_name: &str, args: &serde_json::Value) -> String {
    if args.as_object().map_or(false, |o| o.is_empty()) {
        return String::new();
    }
    let keys: &[&str] = match tool_name {
        "bash" | "read_only_bash" => &["command"],
        "read_file" | "write_file" | "edit_file" | "multi_edit" => &["path"],
        "grep" => &["pattern", "path"],
        "glob" | "code_index" => &["pattern"],
        "web_fetch" => &["url"],
        "ls" => &["path"],
        "Task" => &["subagent_name"],
        _ => &["path", "command", "pattern", "url", "name"],
    };
    for key in keys {
        if let Some(val) = args.get(key) {
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    let json = serde_json::to_string(args).unwrap_or_default();
    if json.len() <= 60 {
        json
    } else {
        format!("{}…", json.chars().take(57).collect::<String>())
    }
}

fn file_path_from_args(args: &serde_json::Value) -> String {
    args.get("file_path")
        .or_else(|| args.get("path"))
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string()
}

/// 从 result 第一段提取 "+N -M" 统计
fn parse_diff_stat(result: &str) -> String {
    // 第二行格式: "/path modified (2 lines added, 1 lines removed)"
    for line in result.lines().skip(1) {
        if let Some(start) = line.find('(') {
            if let Some(end) = line.rfind(')') {
                let inner = &line[start + 1..end];
                let stat: String = inner
                    .replace("lines added", "+")
                    .replace("lines removed", "-")
                    .replace("line added", "+")
                    .replace("line removed", "-")
                    .replace(", ", " ");
                if !stat.is_empty() {
                    return stat;
                }
            }
        }
    }
    String::new()
}

/// 解析 unified diff 并渲染为带行号和颜色标记的视图
fn render_diff_view(result: &str) -> Element {
    // 找到 diff 起始位置（"--- " 行）
    let diff_start = result
        .lines()
        .position(|l| l.starts_with("--- ") || l.starts_with("@@"))
        .unwrap_or(0);
    let diff_lines: Vec<&str> = result.lines().skip(diff_start).collect();
    if diff_lines.is_empty() {
        return rsx! { div { class: "diff-view", pre { class: "diff-view-body", "{result}" } } };
    }

    // 跳过 "--- " 和 "+++ " 头部行，从 @@ 或实际 diff 行开始
    let body_start = diff_lines
        .iter()
        .position(|l| l.starts_with("@@") || !l.starts_with("--- ") && !l.starts_with("+++ "))
        .unwrap_or(0);
    let header_lines = &diff_lines[..body_start];
    let body_lines = &diff_lines[body_start..];

    // 计算行号
    let (mut old_line, mut new_line): (usize, usize) = (1, 1);
    // 尝试从 @@ 行解析起始行号
    if let Some(hunk) = body_lines.first() {
        if let Some(start) = hunk.find("-") {
            let rest = &hunk[start..];
            if let Some(comma) = rest.find(',') {
                if let Ok(n) = rest[1..comma].parse::<usize>() {
                    old_line = n;
                }
            }
            if let Some(plus) = rest.find('+') {
                let after_plus = &rest[plus + 1..];
                let after_plus = after_plus.split(&[',', ' '][..]).next().unwrap_or("1");
                if let Ok(n) = after_plus.parse::<usize>() {
                    new_line = n;
                }
            }
        }
    }

    let mut rows: Vec<DiffRow> = Vec::new();
    for line in body_lines {
        if line.starts_with("@@") {
            rows.push(DiffRow {
                kind: DiffLineKind::Hunk,
                old_num: None,
                new_num: None,
                text: line.to_string(),
            });
            continue;
        }
        let (kind, text) = if line.starts_with('+') {
            (DiffLineKind::Add, &line[1..])
        } else if line.starts_with('-') {
            (DiffLineKind::Del, &line[1..])
        } else if line.starts_with(' ') {
            (DiffLineKind::Ctx, &line[1..])
        } else {
            (DiffLineKind::Ctx, &line[..])
        };
        let (on, nn) = match kind {
            DiffLineKind::Add => {
                let n = new_line;
                new_line += 1;
                (None, Some(n))
            }
            DiffLineKind::Del => {
                let n = old_line;
                old_line += 1;
                (Some(n), None)
            }
            DiffLineKind::Ctx => {
                let o = old_line;
                let n = new_line;
                old_line += 1;
                new_line += 1;
                (Some(o), Some(n))
            }
            DiffLineKind::Hunk => (None, None),
        };
        rows.push(DiffRow {
            kind,
            old_num: on,
            new_num: nn,
            text: text.to_string(),
        });
    }

    rsx! {
        div { class: "diff-view",
            if !header_lines.is_empty() {
                div { class: "diff-view-header",
                    for line in header_lines {
                        div { class: "diff-view-header-line", "{line}" }
                    }
                }
            }
            div { class: "diff-view-body",
                for row in rows {
                    div {
                        class: "diff-row {row.kind.class_name()}",
                        span { class: "diff-num diff-num-old", "{row.old_num.map_or(String::new(), |n| n.to_string())}" }
                        span { class: "diff-num diff-num-new", "{row.new_num.map_or(String::new(), |n| n.to_string())}" }
                        span { class: "diff-text", "{row.text}" }
                    }
                }
            }
        }
    }
}

enum DiffLineKind {
    Add,
    Del,
    Ctx,
    Hunk,
}
impl DiffLineKind {
    fn class_name(&self) -> &'static str {
        match self {
            DiffLineKind::Add => "diff-add",
            DiffLineKind::Del => "diff-del",
            DiffLineKind::Ctx => "diff-ctx",
            DiffLineKind::Hunk => "diff-hunk",
        }
    }
}

struct DiffRow {
    kind: DiffLineKind,
    old_num: Option<usize>,
    new_num: Option<usize>,
    text: String,
}
