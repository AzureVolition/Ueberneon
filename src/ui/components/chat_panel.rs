use dioxus::desktop::use_window;
use dioxus::prelude::*;

use crate::ui::state::{ChatMessage, Role, StreamSegment, ToolCallRecord, ToolCallStatus};

/// 对话面板 —— 消息列表 + 流式输出 + 空状态 + 时序导航
#[component]
pub fn ChatPanel(
    messages: Signal<Vec<ChatMessage>>,
    streaming_segments: Signal<Vec<StreamSegment>>,
    is_streaming: Signal<bool>,
    active_tool_calls: Signal<Vec<ToolCallRecord>>,
    markdown_to_html: fn(&str) -> String,
) -> Element {
    let msgs = messages.read();
    let segments = streaming_segments.read();
    let running = is_streaming();
    let active_calls = active_tool_calls.read();
    let window = use_window();
    let win = window.clone();
    let win2 = win.clone();

    // 收集所有用户消息的索引和内容预览
    let user_messages: Vec<(usize, String)> = msgs
        .iter()
        .enumerate()
        .filter(|(_, m)| matches!(m.role, Role::User))
        .map(|(i, m)| {
            let text = m.content.trim();
            let preview: String = text.chars().take(60).collect();
            let preview = if text.chars().count() > 60 {
                format!("{preview}…")
            } else {
                preview
            };
            (i, preview)
        })
        .collect();
    let last_user_index = user_messages.last().map(|(i, _)| *i);

    // 注入滚动监听：自动高亮当前可见消息对应的 timeline hit
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
// auto-scroll during streaming — MutationObserver-driven, no Rust bridge jitter
(function(){
var p=document.querySelector('.chat-panel');if(!p)return;
var wasStreaming=false;
var ob=new MutationObserver(function(){
// auto-scroll during streaming
if(p._autoFollow!==0){
requestAnimationFrame(function(){
p.scrollTo({top:p.scrollHeight,behavior:'auto'});
});
}
var now=!!document.querySelector('.message-assistant.streaming');
// streaming started → sync bubble animation with timeline
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
// streaming ended → trigger warm→cool cooldown
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

    rsx! {
        div {
            class: "chat-panel",

            if msgs.is_empty() && segments.is_empty() {
                div {
                    class: "chat-empty",
                    span { class: "empty-eyebrow", "01 · CHAT" }
                    h2 {
                        dangerous_inner_html: "ready to <em>think</em> with you."
                    }
                    p { "start a conversation — type your message below." }
                }
            }

            {msgs.iter().enumerate().map(|(i, msg)| {
                let formatted_time = msg.timestamp.format("%H:%M:%S").to_string();
                let (role_label, role_class) = match msg.role {
                    Role::User => ("USER", "user-role"),
                    Role::Assistant => ("ASSISTANT", ""),
                    Role::System => ("SYSTEM", ""),
                };
                let bubble_class = match msg.role {
                    Role::User => "message-bubble message-user",
                    Role::Assistant => "message-bubble message-assistant",
                    Role::System => "message-bubble message-system",
                };
                let msg_id = format!("msg-{i}");
                let content_html = markdown_to_html(&msg.content);
                let tool_calls_html: Vec<_> = msg.tool_calls.iter().map(|call| {
                    let sc = status_class(&call.status);
                    let status_text = match call.status {
                        ToolCallStatus::Running => "running",
                        ToolCallStatus::Success => "success",
                        ToolCallStatus::Failed(_) => "failed",
                    };
                    let args_summary = tool_args_summary(&call.tool_name, &call.args);
                    let result_html = call.result.as_ref().map(|r| {
                        format!("<pre class=\"tool-call-result\">{}</pre>", html_escape(r))
                    }).unwrap_or_default();
                    format!(
                        "<details class=\"tool-call-details {sc}\"><summary class=\"tool-call-summary\"><span class=\"tool-call-name\">{}</span><span class=\"tool-call-args\">{}</span><span class=\"tool-call-status {sc}\">{}</span></summary>{}</details>",
                        html_escape(&call.tool_name), html_escape(&args_summary), status_text, result_html
                    )
                }).collect();

                let tool_calls_section = if tool_calls_html.is_empty() {
                    String::new()
                } else {
                    format!("<div class=\"tool-calls\">{}</div>", tool_calls_html.join(""))
                };

                let full_html = format!(
                    "<div class=\"message-header\"><span class=\"message-role {}\">{}</span><span class=\"message-time\">{}</span></div><div class=\"message-content\">{}</div>{}",
                    role_class, role_label, formatted_time, content_html, tool_calls_section
                );

                let reasoning_html = if !msg.reasoning.is_empty() {
                    markdown_to_html(&msg.reasoning)
                } else {
                    String::new()
                };

                rsx! {
                    div {
                        key: "{i}",
                        id: "{msg_id}",
                        class: bubble_class,

                        // 有 segments 时按 LLM 返回顺序渲染，否则 fallback 到旧格式
                        if !msg.segments.is_empty() {
                            {render_segments(false, &msg.segments, &msg.tool_calls, markdown_to_html).into_iter()}
                        } else {
                            // 旧格式兼容
                            if !reasoning_html.is_empty() {
                                details {
                                    class: "thinking-section",
                                    summary {
                                        class: "thinking-toggle",
                                        "thinking"
                                    }
                                    div {
                                        class: "thinking-content",
                                        dangerous_inner_html: reasoning_html,
                                    }
                                }
                            }
                            div {
                                dangerous_inner_html: full_html,
                            }
                        }
                    }
                }
            })}

            // 流式输出区 —— 按 LLM 返回的 StreamSegment 顺序渲染
            if running && !segments.is_empty() {
                div {
                    key: "{streaming_key}",
                    class: "message-bubble message-assistant streaming",
                    {render_segments(true, &segments, &active_calls, markdown_to_html).into_iter()}
                }
            }

            if running && segments.is_empty() {
                div {
                    class: "message-bubble message-assistant thinking",
                    div {
                        class: "thinking-dots",
                        span { "." }
                        span { "." }
                        span { "." }
                    }
                }
            }

            // ── Minimap heatbar ──
            if !user_messages.is_empty() {
                div {
                    class: "chat-timeline",
                    {user_messages.iter().map(|(idx, preview)| {
                        let i = *idx;
                        let tooltip = preview.clone();
                        let w = win.clone();
                        rsx! {
                            div {
                                class: if running && Some(*idx) == last_user_index {{ "timeline-hit streaming" }} else {{ "timeline-hit" }},
                                "data-index": "{idx}",
                                title: "{tooltip}",
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

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 将连续的 Reasoning + ToolCall segments 归组到可折叠的 think-watch 块中
fn render_segments(
    streaming: bool,
    segments: &[StreamSegment],
    tool_calls: &[ToolCallRecord],
    markdown_to_html: fn(&str) -> String,
) -> Vec<Element> {
    let mut items: Vec<Element> = Vec::new();
    let mut tc_idx = 0usize;
    let mut buf: Vec<Element> = Vec::new();

    let mut flush = |buf: &mut Vec<Element>, items: &mut Vec<Element>| {
        if !buf.is_empty() {
            let children: Vec<Element> = buf.drain(..).collect();
            items.push(rsx! {
                details {
                    class: "think-watch",
                    open: streaming,
                    summary { class: "think-watch-toggle", "think watch write" }
                    {children.into_iter()}
                }
            });
        }
    };

    for seg in segments {
        match seg {
            StreamSegment::Text(t) => {
                flush(&mut buf, &mut items);
                items.push(rsx! {
                    div {
                        class: "message-content",
                        dangerous_inner_html: markdown_to_html(t),
                    }
                });
            }
            StreamSegment::Reasoning(text) => {
                let html = markdown_to_html(text);
                buf.push(rsx! {
                    div {
                        class: "thinking-content",
                        dangerous_inner_html: html,
                    }
                });
            }
            StreamSegment::ToolCall => {
                if let Some(call) = tool_calls.get(tc_idx) {
                    let sc = status_class(&call.status);
                    let status_text = match call.status {
                        ToolCallStatus::Running => "running",
                        ToolCallStatus::Success => "success",
                        ToolCallStatus::Failed(_) => "failed",
                    };
                    buf.push(rsx! {
                        details {
                            class: "tool-call-details {sc}",
                            summary {
                                class: "tool-call-summary",
                                span { class: "tool-call-name", "{call.tool_name}" }
                                span {
                                    class: "tool-call-args",
                                    "{tool_args_summary(&call.tool_name, &call.args)}"
                                }
                                span {
                                    class: "tool-call-status {sc}",
                                    "{status_text}"
                                }
                            }
                            if let Some(ref result) = call.result {
                                pre { class: "tool-call-result", "{result}" }
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

/// 从工具调用状态获取 CSS class 后缀
fn status_class(status: &ToolCallStatus) -> &'static str {
    match status {
        ToolCallStatus::Running => "status-running",
        ToolCallStatus::Success => "status-success",
        ToolCallStatus::Failed(_) => "status-failed",
    }
}

/// 从工具调用的参数中提取简短摘要（用于在 summary 中显示入参）
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
            if let Some(s) = val.as_str() {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
    }
    let json = serde_json::to_string(args).unwrap_or_default();
    if json.len() <= 60 { json } else { format!("{}…", &json[..57]) }
}
