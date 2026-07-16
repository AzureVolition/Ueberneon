# racpagent 桌面端集成方案 — Dioxus

## 1. 架构总览

```
┌──────────────────────────────────────────────────────────┐
│                   Dioxus Desktop App                      │
│                                                          │
│  ┌─────────────┐    Signal 读写      ┌────────────────┐  │
│  │  Components │ ◄─────────────────► │  use_signal()   │  │
│  │  (rsx! 宏)  │    响应式重渲染      │  (全局状态)      │  │
│  └──────┬──────┘                     └───────┬────────┘  │
│         │ 用户事件 (onclick/onsubmit)         │           │
│         │ spawn(async { ... })               │           │
│         ▼                                    │           │
│  ┌──────────────┐                            │           │
│  │  bridge.rs   │── LLM 流式 token ──► signal.set()       │
│  │ run_agent_loop│                           │           │
│  └──────┬───────┘                            │           │
│         │                                    │           │
└─────────┼────────────────────────────────────┼───────────┘
          │                                    │
  ┌───────▼────────────────────────────────────▼───────────┐
  │                    现有 racpagent                       │
  │   agent / tools / permission / llm / models            │
  │   (零修改)                                              │
  └────────────────────────────────────────────────────────┘
```

**核心原则**：现有代码零修改，UI 层通过 Dioxus `Signal` + `spawn` 在上层驱动 agent 循环。

---

## 2. 技术选型

| 项目 | 选择 | 理由 |
|------|------|------|
| UI 框架 | Dioxus 0.7.9 | 最新稳定版，React-like RSX 语法，`Signal` 状态管理 |
| 桌面渲染 | `dioxus-desktop` (feature) | 基于 wry/tao 系统 WebView，打包 < 5MB |
| 状态管理 | `use_signal` / `Signal<T>` | Dioxus 原生，`Copy` 语义，可直接移入 async 闭包 |
| 异步 | `spawn()` | 内联到事件处理器，自动取消，无额外 channel |
| Markdown | `pulldown-cmark` | Rust 原生，渲染为 HTML 后在 Dioxus 中通过 `dangerous_inner_html` 显示 |
| 样式 | `stylesheet!` 宏 / `head::style` | 无外部 CSS 文件依赖 |
| 现有代码修改量 | **零** | 所有现有模块保持不动 |

---

## 3. 目录结构

```
src/
├── main.rs                  # → Dioxus desktop 入口 (dioxus::launch)
├── lib.rs                   # (不变)
├── agent/                   # (不变)
├── models/                  # (不变)
├── permission/              # (不变)
├── tools/                   # (不变)
├── llm/                     # (不变)
│
└── ui/                      # 【新增】Dioxus UI 层
    ├── mod.rs               # re-exports
    ├── state.rs             # 状态类型：ChatMessage, ToolCallRecord, Conversation, AppConfig
    ├── bridge.rs            # agent 循环 → UI signals 桥接函数
    │
    └── components/
        ├── mod.rs
        ├── app.rs           # 根组件，初始化全局 signal + 布局
        ├── sidebar.rs       # 对话列表 + 新建对话
        ├── chat_panel.rs    # 消息列表 + 流式指示
        ├── message_bubble.rs # 单条消息气泡 (含工具调用折叠卡片)
        └── input_bar.rs     # 输入框 + 发送/取消 + 模式切换
```

---

## 4. 核心数据模型 (`state.rs`)

```rust
// src/ui/state.rs

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

/// 角色
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 工具调用记录
#[derive(Clone)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result: Option<String>,
    pub status: ToolCallStatus,
}

#[derive(Clone, PartialEq)]
pub enum ToolCallStatus {
    Running,
    Success,
    Failed(String),
}

/// 一条聊天消息
#[derive(Clone)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Local>,
    pub tool_calls: Vec<ToolCallRecord>,
}

/// 对话
#[derive(Clone)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
}

/// 应用配置
#[derive(Clone)]
pub struct AppConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub agent_mode: String,   // "ask" | "auto" | "cautious" | "unrestrained"
}
```

---

## 5. Agent 桥接层 (`bridge.rs`)

关键设计：`Signal<T>` 是 `Copy` 的，可以直接被移入 `spawn` 的 async 闭包中，实现异步任务与 UI 的无缝通信。

```rust
// src/ui/bridge.rs

use dioxus::prelude::*;
use crate::ui::state::*;
use crate::agent::AgentMode;
use llm::{OpenAiProvider, Provider, Request, Message, Role as LlmRole, Chunk};
use racpagent::tools::Registry;

pub async fn run_agent_loop(
    user_input: String,
    config: AppConfig,
    mut messages: Signal<Vec<ChatMessage>>,
    mut streaming_content: Signal<String>,
    mut is_streaming: Signal<bool>,
    mut active_tool_calls: Signal<Vec<ToolCallRecord>>,
) {
    is_streaming.set(true);
    streaming_content.set(String::new());

    // 1. 构建 LLM provider
    let provider = match OpenAiProvider::new(
        "custom".into(),
        config.base_url,
        config.model,
        config.api_key,
        None,
        false,
        None,
    ) {
        Ok(p) => p,
        Err(e) => {
            streaming_content.set(format!("Provider error: {e}"));
            is_streaming.set(false);
            return;
        }
    };

    // 2. 构建工具注册表
    let registry = Registry::new();
    racpagent::tools::register_builtins(&registry);

    // 3. 构建消息历史
    let msgs = messages.read();
    let mut llm_messages: Vec<Message> = vec![
        Message {
            role: LlmRole::System,
            content: Some("You are a helpful assistant.".into()),
            ..Default::default()
        },
    ];
    for m in msgs.iter() {
        llm_messages.push(Message {
            role: match m.role {
                Role::User => LlmRole::User,
                Role::Assistant => LlmRole::Assistant,
                Role::System => LlmRole::System,
            },
            content: Some(m.content.clone()),
            ..Default::default()
        });
    }
    llm_messages.push(Message {
        role: LlmRole::User,
        content: Some(user_input.clone()),
        ..Default::default()
    });

    let mut req = Request {
        messages: llm_messages,
        tools: registry.schemas(),
        temperature: config.temperature,
        max_tokens: config.max_tokens,
    };

    // 4. Agent 循环
    loop {
        let mut have_tool_calls = false;
        let stream = match provider.stream(&req).await {
            Ok(s) => s,
            Err(e) => {
                streaming_content.set(format!("Stream error: {e}"));
                break;
            }
        };

        use futures::StreamExt;
        tokio::pin!(stream);

        let mut output = String::new();
        let mut pending_tool_calls: Vec<llm::ToolCall> = Vec::new();

        while let Some(result) = stream.next().await {
            match result {
                Ok(Chunk::Text(t)) => {
                    output.push_str(&t);
                    streaming_content.set(output.clone());
                }
                Ok(Chunk::ToolCallComplete(tool)) => {
                    have_tool_calls = true;
                    pending_tool_calls.push(tool);
                }
                Err(e) => {
                    streaming_content.set(format!("Error: {e}"));
                    break;
                }
                _ => {}
            }
        }

        // push assistant 消息
        {
            let mut msg = Message {
                role: LlmRole::Assistant,
                content: Some(output.clone()),
                ..Default::default()
            };
            if !pending_tool_calls.is_empty() {
                msg.tool_calls = pending_tool_calls.clone();
            }
            req.messages.push(msg);
        }

        // 执行工具
        for tool in &pending_tool_calls {
            if let Some(t) = registry.get(&tool.name) {
                use racpagent::agent::ToolContext;
                let ctx = ToolContext {
                    call_id: tool.id.clone(),
                    plan_mode: PlanMode::Regular,
                    agent_mode: AgentMode::Ask,
                    progress: None,
                };
                let args: serde_json::Value =
                    serde_json::from_str(&tool.arguments).unwrap_or_default();

                // 更新活跃工具调用
                active_tool_calls.write().push(ToolCallRecord {
                    tool_name: tool.name.clone(),
                    args: args.clone(),
                    result: None,
                    status: ToolCallStatus::Running,
                });

                let result = t.checked_execute(&ctx, &args).await;

                // 更新工具结果
                active_tool_calls.write().iter_mut()
                    .filter(|tc| tc.tool_name == tool.name && tc.status == ToolCallStatus::Running)
                    .for_each(|tc| {
                        tc.result = Some(if let Ok(ref tr) = result {
                            tr.output.clone()
                        } else {
                            result.as_ref().err().cloned().unwrap_or_default()
                        });
                        tc.status = match &result {
                            Ok(_) => ToolCallStatus::Success,
                            Err(e) => ToolCallStatus::Failed(e.clone()),
                        };
                    });

                req.messages.push(Message {
                    role: LlmRole::Tool,
                    content: Some(result.output().to_string()),
                    tool_call_id: Some(tool.id.clone()),
                    name: Some(tool.name.clone()),
                    ..Default::default()
                });
            }
        }

        if !have_tool_calls {
            // 最终响应追加到消息列表
            let final_content = streaming_content.read().clone();
            if !final_content.is_empty() {
                messages.write().push(ChatMessage {
                    role: Role::Assistant,
                    content: final_content,
                    timestamp: chrono::Local::now(),
                    tool_calls: active_tool_calls.read().clone(),
                });
            }
            break;
        }
    }

    streaming_content.set(String::new());
    is_streaming.set(false);
}
```

**关键点**：
- `Signal<T>` 实现 `Copy`，可移入 `spawn` 的 `async move {}` 闭包
- `signal.read()` 返回 `ReadableRef`，`signal.write()` 返回 `WritableRef`
- 无需 `EventProxy` 或 channel —— 直接在异步任务中读写 signal，Dioxus 自动触发重渲染

---

## 6. UI 组件树

### 入口 `main.rs`

```rust
// src/main.rs

use dioxus::prelude::*;
use racpagent::ui::components::app::App;

fn main() {
    dotenvy::dotenv().ok();

    dioxus::launch(App);
}
```

### 根组件 `app.rs`

```rust
// src/ui/components/app.rs

use dioxus::prelude::*;
use crate::ui::state::*;
use crate::ui::components::sidebar::Sidebar;
use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::input_bar::InputBar;

#[component]
pub fn App() -> Element {
    // ── 全局状态 ──
    let conversations = use_signal(|| vec![Conversation {
        id: "default".into(),
        title: "新对话".into(),
        messages: vec![],
    }]);
    let active_conversation_id = use_signal(|| "default".to_string());
    let messages = use_signal(Vec::<ChatMessage>::new);
    let streaming_content = use_signal(String::new);
    let is_streaming = use_signal(|| false);
    let active_tool_calls = use_signal(Vec::<ToolCallRecord>::new);
    let config = use_signal(|| AppConfig {
        model: std::env::var("OPENAI_MODEL").unwrap_or_else(|_| "deepseek-chat".into()),
        base_url: std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.deepseek.com".into()),
        api_key: std::env::var("OPENAI_API_KEY").unwrap_or_default(),
        temperature: 0.7,
        max_tokens: 8192,
        agent_mode: "ask".into(),
    });

    rsx! {
        document::Link {
            rel: "stylesheet",
            href: "https://fonts.googleapis.com/css2?family=Inter:wght@400;500;600&display=swap"
        }
        style { {include_str!("style.css")} }

        div {
            class: "app-container",
            Sidebar {
                conversations,
                active_conversation_id,
                on_new_conversation: move |_| {
                    // TODO: 新建对话
                },
            }
            div {
                class: "main-area",
                ChatPanel {
                    messages,
                    streaming_content,
                    is_streaming,
                    active_tool_calls,
                }
                InputBar {
                    messages,
                    streaming_content,
                    is_streaming,
                    active_tool_calls,
                    config,
                    on_send: move |input: String| {
                        let config_val = config.read().clone();
                        spawn(async move {
                            crate::ui::bridge::run_agent_loop(
                                input,
                                config_val,
                                messages,
                                streaming_content,
                                is_streaming,
                                active_tool_calls,
                            ).await;
                        });
                    },
                }
            }
        }
    }
}
```

### 对话面板 `chat_panel.rs`

```rust
// src/ui/components/chat_panel.rs

use dioxus::prelude::*;
use crate::ui::state::*;
use crate::ui::components::message_bubble::MessageBubble;

#[component]
pub fn ChatPanel(
    messages: Signal<Vec<ChatMessage>>,
    streaming_content: Signal<String>,
    is_streaming: Signal<bool>,
    active_tool_calls: Signal<Vec<ToolCallRecord>>,
) -> Element {
    let scroll_target = use_signal(|| 0u32);

    // 新消息时自动滚动到底部
    use_effect(move || {
        let _ = messages.read();
        let _ = streaming_content.read();
        scroll_target.set(scroll_target() + 1);
    });

    let msgs = messages.read();
    let streaming = streaming_content.read();
    let running = is_streaming();

    rsx! {
        div {
            class: "chat-panel",
            id: "chat-scroll",
            scroll_behavior: "smooth",

            // 空状态
            if msgs.is_empty() && streaming.is_empty() {
                div {
                    class: "chat-empty",
                    h2 { "RACP Agent" }
                    p { "输入消息开始对话..." }
                }
            }

            // 消息列表
            for msg in msgs.iter() {
                MessageBubble { message: msg.clone() }
            }

            // 流式输出
            if running && !streaming.is_empty() {
                div {
                    class: "message-bubble message-assistant streaming",
                    div { class: "message-content", dangerous_inner_html: &markdown_to_html(&streaming) }
                    div { class: "streaming-indicator", "▊" }
                }
            }

            // 思考中指示
            if running && streaming.is_empty() {
                div {
                    class: "message-bubble message-assistant thinking",
                    div { class: "thinking-dots",
                        span { "." }
                        span { "." }
                        span { "." }
                    }
                }
            }
        }
    }
}
```

### 消息气泡 `message_bubble.rs`

```rust
// src/ui/components/message_bubble.rs

use dioxus::prelude::*;
use crate::ui::state::*;

#[component]
pub fn MessageBubble(message: ChatMessage) -> Element {
    let class = match message.role {
        Role::User => "message-bubble message-user",
        Role::Assistant => "message-bubble message-assistant",
        Role::System => "message-bubble message-system",
    };

    rsx! {
        div { class: "{class}",
            div { class: "message-header",
                span { class: "message-role",
                    match message.role {
                        Role::User => "You",
                        Role::Assistant => "Assistant",
                        Role::System => "System",
                    }
                }
                span { class: "message-time",
                    "{message.timestamp.format(\"%H:%M:%S\")}"
                }
            }
            div {
                class: "message-content",
                dangerous_inner_html: &markdown_to_html(&message.content),
            }

            // 工具调用卡片
            if !message.tool_calls.is_empty() {
                div { class: "tool-calls",
                    for call in message.tool_calls.iter() {
                        div { class: "tool-call-card",
                            div { class: "tool-call-header",
                                span { class: "tool-call-name", "🔧 {call.tool_name}" }
                                span { class: "tool-call-status",
                                    match call.status {
                                        ToolCallStatus::Running => "⏳ Running",
                                        ToolCallStatus::Success => "✅ Success",
                                        ToolCallStatus::Failed(_) => "❌ Failed",
                                    }
                                }
                            }
                            if let Some(ref result) = call.result {
                                pre { class: "tool-call-result", "{result}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
```

### 输入栏 `input_bar.rs`

```rust
// src/ui/components/input_bar.rs

use dioxus::prelude::*;
use crate::ui::state::*;

#[component]
pub fn InputBar(
    messages: Signal<Vec<ChatMessage>>,
    streaming_content: Signal<String>,
    is_streaming: Signal<bool>,
    active_tool_calls: Signal<Vec<ToolCallRecord>>,
    config: Signal<AppConfig>,
    on_send: EventHandler<String>,
) -> Element {
    let mut input = use_signal(String::new);
    let running = is_streaming();

    let handle_send = move |_| {
        let text = input.read().trim().to_string();
        if text.is_empty() || running {
            return;
        }

        // 添加用户消息
        messages.write().push(ChatMessage {
            role: Role::User,
            content: text.clone(),
            timestamp: chrono::Local::now(),
            tool_calls: vec![],
        });

        let send_text = text;
        input.set(String::new());
        on_send.call(send_text);
    };

    let handle_cancel = move |_| {
        is_streaming.set(false);
        streaming_content.set(String::new());
    };

    rsx! {
        div { class: "input-bar",
            div { class: "input-row",
                textarea {
                    class: "input-textarea",
                    value: "{input}",
                    placeholder: "输入消息... (Enter 发送, Shift+Enter 换行)",
                    disabled: running,
                    rows: 2,
                    oninput: move |evt| input.set(evt.value()),
                    onkeydown: move |evt| {
                        if evt.key() == Key::Enter && !evt.modifiers().contains(Modifiers::SHIFT) {
                            evt.prevent_default();
                            handle_send(());
                        }
                    },
                }
                if running {
                    button {
                        class: "btn btn-cancel",
                        onclick: handle_cancel,
                        "取消"
                    }
                } else {
                    button {
                        class: "btn btn-send",
                        onclick: handle_send,
                        "发送"
                    }
                }
            }
        }
    }
}
```

### 侧边栏 `sidebar.rs`

```rust
// src/ui/components/sidebar.rs

use dioxus::prelude::*;
use crate::ui::state::*;

#[component]
pub fn Sidebar(
    conversations: Signal<Vec<Conversation>>,
    active_conversation_id: Signal<String>,
    on_new_conversation: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "sidebar",
            div { class: "sidebar-header",
                h3 { "RACP Agent" }
                button {
                    class: "btn btn-new-chat",
                    onclick: move |_| on_new_conversation.call(()),
                    "+ 新对话"
                }
            }
            div { class: "conversation-list",
                for conv in conversations.read().iter() {
                    div {
                        class: if conv.id == *active_conversation_id.read() {
                            "conversation-item active"
                        } else {
                            "conversation-item"
                        },
                        onclick: move |_| {
                            active_conversation_id.set(conv.id.clone());
                        },
                        span { class: "conversation-title", "{conv.title}" }
                    }
                }
            }
        }
    }
}
```

---

## 7. 样式方案 (`style.css`)

样式通过 `include_str!` 内联到二进制中，无需外部文件：

```css
/* 全局 */
* {
    margin: 0;
    padding: 0;
    box-sizing: border-box;
}

body {
    font-family: 'Inter', -apple-system, BlinkMacSystemFont, sans-serif;
    font-size: 14px;
    background: #1a1a2e;
    color: #e0e0e0;
    overflow: hidden;
}

/* 布局 */
.app-container {
    display: flex;
    height: 100vh;
    width: 100vw;
}

.sidebar {
    width: 260px;
    background: #16213e;
    border-right: 1px solid #2a2a4a;
    display: flex;
    flex-direction: column;
    flex-shrink: 0;
}

.main-area {
    flex: 1;
    display: flex;
    flex-direction: column;
    min-width: 0;
}

/* 对话面板 */
.chat-panel {
    flex: 1;
    overflow-y: auto;
    padding: 20px;
    scroll-behavior: smooth;
}

.chat-empty {
    display: flex;
    flex-direction: column;
    align-items: center;
    justify-content: center;
    height: 100%;
    opacity: 0.5;
}
.chat-empty h2 { font-size: 24px; margin-bottom: 8px; }

/* 消息气泡 */
.message-bubble {
    max-width: 80%;
    margin-bottom: 16px;
    padding: 12px 16px;
    border-radius: 12px;
    line-height: 1.6;
    animation: fadeIn 0.2s ease-in;
}

.message-user {
    margin-left: auto;
    background: #2d6ff7;
    color: #fff;
}

.message-assistant {
    margin-right: auto;
    background: #1e2a4a;
    border: 1px solid #2a2a4a;
}

.message-header {
    display: flex;
    justify-content: space-between;
    font-size: 12px;
    margin-bottom: 6px;
    opacity: 0.7;
}

.message-content {
    word-break: break-word;
}

.message-content pre {
    background: #0d1117;
    border-radius: 6px;
    padding: 12px;
    margin: 8px 0;
    overflow-x: auto;
    font-size: 13px;
}

.message-content code {
    font-family: 'JetBrains Mono', 'Fira Code', monospace;
    font-size: 13px;
}

.message-content p { margin: 4px 0; }
.message-content ul, .message-content ol { margin: 8px 0; padding-left: 20px; }

/* 流式输出 */
.streaming .message-content::after {
    content: "▊";
    animation: blink 0.8s infinite;
}

.thinking-dots span {
    animation: dotPulse 1.4s infinite;
    font-size: 24px;
}
.thinking-dots span:nth-child(2) { animation-delay: 0.2s; }
.thinking-dots span:nth-child(3) { animation-delay: 0.4s; }

/* 工具调用卡片 */
.tool-calls { margin-top: 8px; }
.tool-call-card {
    background: #1a1a30;
    border: 1px solid #2a2a4a;
    border-radius: 8px;
    padding: 10px;
    margin-top: 6px;
}
.tool-call-header {
    display: flex;
    justify-content: space-between;
    font-size: 13px;
    margin-bottom: 6px;
}
.tool-call-name { color: #4fc3f7; }
.tool-call-result {
    background: #0d1117;
    border-radius: 4px;
    padding: 8px;
    font-size: 12px;
    max-height: 150px;
    overflow-y: auto;
    white-space: pre-wrap;
}

/* 输入栏 */
.input-bar {
    padding: 16px 20px;
    border-top: 1px solid #2a2a4a;
    background: #16213e;
}

.input-row {
    display: flex;
    gap: 10px;
    align-items: flex-end;
}

.input-textarea {
    flex: 1;
    background: #1a1a30;
    border: 1px solid #2a2a4a;
    border-radius: 8px;
    color: #e0e0e0;
    font-size: 14px;
    padding: 10px 14px;
    resize: none;
    font-family: inherit;
    outline: none;
    transition: border-color 0.2s;
}
.input-textarea:focus { border-color: #2d6ff7; }
.input-textarea:disabled { opacity: 0.5; }

/* 按钮 */
.btn {
    padding: 10px 20px;
    border: none;
    border-radius: 8px;
    font-size: 14px;
    font-weight: 500;
    cursor: pointer;
    transition: background 0.2s, opacity 0.2s;
    white-space: nowrap;
}
.btn-send { background: #2d6ff7; color: #fff; }
.btn-send:hover { background: #1a5cf7; }
.btn-cancel { background: #d32f2f; color: #fff; }
.btn-cancel:hover { background: #b71c1c; }

/* 侧边栏 */
.sidebar-header {
    padding: 20px;
    border-bottom: 1px solid #2a2a4a;
}
.sidebar-header h3 { margin-bottom: 12px; font-size: 16px; }
.btn-new-chat {
    width: 100%;
    background: #2d6ff7;
    color: #fff;
}
.conversation-list { flex: 1; overflow-y: auto; padding: 10px; }
.conversation-item {
    padding: 10px 12px;
    border-radius: 6px;
    cursor: pointer;
    font-size: 13px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
}
.conversation-item:hover { background: #2a2a4a; }
.conversation-item.active { background: #2d6ff7; }

/* 动画 */
@keyframes fadeIn {
    from { opacity: 0; transform: translateY(8px); }
    to { opacity: 1; transform: translateY(0); }
}
@keyframes blink {
    0%, 100% { opacity: 1; }
    50% { opacity: 0; }
}
@keyframes dotPulse {
    0%, 80%, 100% { opacity: 0; }
    40% { opacity: 1; }
}
```

---

## 8. 依赖变更 (`Cargo.toml`)

```toml
[dependencies]
# ... 现有依赖保持不变 ...

# 新增
dioxus = { version = "0.7", features = ["desktop"] }
pulldown-cmark = "0.12"
```

注意：
- Dioxus 0.7 使用 `edition = "2021"`，你的项目是 `edition = "2024"`，两者兼容无问题
- `dioxus` 的 `desktop` feature 自动引入 `dioxus-desktop`（基于 wry/tao），macOS 上直接可用
- 不需要额外引入 `tokio` —— Dioxus desktop 自带 tokio runtime

---

## 9. 实施路线图

| 阶段 | 内容 | 预计工作量 |
|------|------|-----------|
| **Phase 1** | 更新 Cargo.toml 依赖，创建 `src/ui/` 模块骨架，重写 `main.rs` 跑通空窗口 | ~30 min |
| **Phase 2** | 实现 `state.rs` 类型 + `bridge.rs` agent 循环桥接 | ~2 h |
| **Phase 3** | 构建所有组件：app、chat_panel、message_bubble、input_bar、sidebar | ~3 h |
| **Phase 4** | 样式打磨：CSS 主题、Markdown 渲染、动画、暗色主题 | ~1 h |
| **Phase 5** | 完善：设置面板、多对话管理、快捷键、错误处理 | ~2 h |

---

## 10. 关键设计决策记录

| 决策 | 选择 | 理由 |
|------|------|------|
| UI 框架 | Dioxus 0.7.9 | 最新稳定版，React-like RSX，生态系统活跃 |
| 入口文件 | `src/main.rs` → `dioxus::launch` | 当前 main.rs 是 stub，直接替换 |
| UI 模块位置 | `src/ui/` | 与现有 lib 模块平级，清晰隔离 |
| 状态管理 | Dioxus `Signal<T>` | 原生 `Copy` 语义，可直接移入 async 闭包 |
| 异步桥接 | `spawn()` + signal 直接读写 | 无 channel、无 EventProxy，零中间层 |
| Markdown 渲染 | `pulldown-cmark` → HTML → `dangerous_inner_html` | Rust 原生，离线可用 |
| 样式 | `include_str!("style.css")` 内联 | 无外部依赖，单二进制分发 |
| 现有代码修改量 | **零** | 所有 `agent/`、`tools/`、`permission/`、`llm/` 模块保持不动 |
