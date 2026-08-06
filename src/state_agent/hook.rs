use std::sync::Arc;

use super::AgentState;
use super::ToolResult;

// ── AgentEvent ───────────────────────────────────────────────────────────────

/// 增量类型（StreamDelta 使用）
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeltaKind {
    /// 文本增量
    Text,
    /// 推理/思考增量
    Reasoning,
}

pub enum AgentEvent {
    /// 用户提交输入后、进入 LLM 前
    UserPromptSubmit { prompt: String },
    /// 流式增量（UI 据此刷新）
    StreamDelta { kind: DeltaKind },
    /// 运行状态变化（变体 A：UI 据此显示精确状态）
    StateChanged { state: AgentState },
    /// 流式阶段收到一个工具调用（前端据此刷新，无需等 execute）
    ToolCallQueued {
        tool_name: String,
        args: serde_json::Value,
    },
    /// 工具开始执行（含审批前的运行态）
    ToolCallStart {
        tool_name: String,
        args: serde_json::Value,
    },
    /// 发起审批请求
    ApprovalRequested {
        tool_name: String,
        args: serde_json::Value,
        reason: String,
    },
    /// 审批决策落地（allow/deny 已写入 record 后广播，UI 据此瞬时刷新审批卡）
    ToolCallDecision { call_id: String, approved: bool },
    /// 工具执行结束（含状态落定）
    ToolCallEnd {
        tool_name: String,
        result: Result<ToolResult, String>,
    },
    /// 执行出错
    Error { message: String },
    /// 工具执行前
    PreToolUse {
        tool_name: String,
        args: serde_json::Value,
    },
    /// 工具执行后
    PostToolUse {
        tool_name: String,
        result: Result<ToolResult, String>,
    },
    /// 循环即将退出时
    Stop { reason: String },
}

impl From<&str> for AgentEvent {
    /// 从字符串标签创建 AgentEvent，数据字段填空值。
    ///
    /// 支持的标签（不区分大小写，- 视为 _）：
    /// - `"prompt"` / `"user_prompt_submit"` → `UserPromptSubmit { prompt: "" }`
    /// - `"pretool"` / `"pre_tool_use"`       → `PreToolUse { tool_name: "", args: Null }`
    /// - `"posttool"` / `"post_tool_use"`     → `PostToolUse { tool_name: "", result: Ok(…) }`
    /// - `"stop"`                              → `Stop { reason: "" }`
    /// - 其他                                  → `Stop { reason: s }`
    fn from(s: &str) -> Self {
        match s.to_lowercase().replace('-', "_").as_str() {
            "prompt" | "user_prompt_submit" => Self::UserPromptSubmit {
                prompt: String::new(),
            },
            "pretool" | "pre_tool_use" => Self::PreToolUse {
                tool_name: String::new(),
                args: serde_json::Value::Null,
            },
            "posttool" | "post_tool_use" => Self::PostToolUse {
                tool_name: String::new(),
                result: Ok(ToolResult {
                    output: String::new(),
                    truncated: false,
                }),
            },
            "stop" => Self::Stop {
                reason: String::new(),
            },
            other => Self::Stop {
                reason: other.to_string(),
            },
        }
    }
}

// ── Hook trait ───────────────────────────────────────────────────────────────

pub trait Hook: Send + Sync {
    fn on_event(&self, event: &AgentEvent);
}

// ── CustomizedHook ───────────────────────────────────────────────────────────

/// 预置的可自定义 Hook，用户可指定监听的事件种类、工具名，以及回调逻辑。
pub struct CustomizedHook {
    listen_event: AgentEvent,
    listen_tools_name: Option<String>,
    callback: Box<dyn Fn(&AgentEvent) + Send + Sync>,
}

impl CustomizedHook {
    pub fn new(
        listen_event: AgentEvent,
        listen_tools_name: Option<String>,
        callback: Box<dyn Fn(&AgentEvent) + Send + Sync>,
    ) -> Self {
        Self {
            listen_event,
            listen_tools_name,
            callback,
        }
    }
}

impl Hook for CustomizedHook {
    fn on_event(&self, event: &AgentEvent) {
        // 用 discriminant 匹配事件种类（忽略数据字段值）
        if std::mem::discriminant(&self.listen_event) != std::mem::discriminant(event) {
            return;
        }

        // 如果有工具名过滤，检查是否匹配
        if let Some(ref name) = self.listen_tools_name {
            let event_tool_name = match event {
                AgentEvent::PreToolUse { tool_name, .. }
                | AgentEvent::PostToolUse { tool_name, .. } => Some(tool_name.as_str()),
                _ => None,
            };
            match event_tool_name {
                Some(n) if n == name.as_str() => {}
                _ => return,
            }
        }

        (self.callback)(event);
    }
}

// ── HookRegister ─────────────────────────────────────────────────────────────

/// 待注入的提示词：hook 经 `push_prompt` 提交，AgentCore 在请求 LLM 前
/// `drain_prompts` 收集注入 messages。`source` 标记来源，供按需撤回
/// （如 stall 催促在 CompleteStep 成功时被 `remove_prompts("stall")` 撤回）。
pub struct PendingPrompt {
    pub message: llm::Message,
    pub source: &'static str,
}

/// 已注册的 hook 条目。
pub enum HookEntry {
    /// 匿名 hook（`register` 注册，不可单独注销）
    Anonymous(Box<dyn Hook>),
    /// 带 id 的 hook（`register_with_id` 注册，`unregister(id)` 可移除）
    Identified {
        id: &'static str,
        hook: Box<dyn Hook>,
    },
}

pub struct HookRegister {
    hooks: Vec<HookEntry>,
    /// 待注入提示词缓冲（hook 内部可变写入，AgentCore 请求前 drain）
    pub(crate) pending_prompts: Arc<std::sync::Mutex<Vec<PendingPrompt>>>,
}

impl HookRegister {
    pub fn new() -> Self {
        Self {
            hooks: Vec::new(),
            pending_prompts: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// 注册 hook（不携带 id，无法单独注销）。
    pub fn register(&mut self, hook: impl Hook + 'static) {
        self.hooks.push(HookEntry::Anonymous(Box::new(hook)));
    }

    /// 注册带 id 的 hook（`unregister(id)` 可单独移除）。
    pub fn register_with_id(&mut self, id: &'static str, hook: impl Hook + 'static) {
        self.hooks.push(HookEntry::Identified {
            id,
            hook: Box::new(hook),
        });
    }

    /// 按 id 移除 hook（幂等；不误删其他 hook）。
    pub fn unregister(&mut self, id: &'static str) {
        self.hooks
            .retain(|e| !matches!(e, HookEntry::Identified { id: eid, .. } if *eid == id));
    }

    /// 是否已注册指定 id 的 hook。
    pub fn has(&self, id: &'static str) -> bool {
        self.hooks
            .iter()
            .any(|e| matches!(e, HookEntry::Identified { id: eid, .. } if *eid == id))
    }

    /// 向所有已注册的 hook 分发事件。
    pub fn emit(&self, event: &AgentEvent) {
        for entry in &self.hooks {
            match entry {
                HookEntry::Anonymous(hook) | HookEntry::Identified { hook, .. } => {
                    hook.on_event(event)
                }
            }
        }
    }

    /// hook 注入提示词（如 stall 催促、策略提示等；多 hook 可各自注入）。
    pub fn push_prompt(&self, source: &'static str, message: llm::Message) {
        self.pending_prompts
            .lock()
            .expect("pending_prompts lock poisoned")
            .push(PendingPrompt { message, source });
    }

    /// 按来源移除待注入提示词（如 CompleteStep 成功后撤回 stall 催促）。
    pub fn remove_prompts(&self, source: &'static str) {
        self.pending_prompts
            .lock()
            .expect("pending_prompts lock poisoned")
            .retain(|p| p.source != source);
    }

    /// 收集并清空所有待注入提示词（AgentCore 在请求 LLM 前调用）。
    pub fn drain_prompts(&self) -> Vec<llm::Message> {
        std::mem::take(
            &mut *self
                .pending_prompts
                .lock()
                .expect("pending_prompts lock poisoned"),
        )
        .into_iter()
        .map(|p| p.message)
        .collect()
    }
}

// ── 业务 hook：计划停滞跟踪（stall 计数与催促注入） ─────────────────────────
//
// callback 逻辑以具名类型实现 `Hook` trait（而非匿名闭包），定义于 hook 模块：
//   StallTracker      每轮 execute 开始（StateChanged{Executing}）stall_count += 1，
//                     >= 3 时向 HookRegister 注入 stall 催促提示词并重置
//   CompleteStepReset CompleteStep 执行成功（PostToolUse + is_ok）stall_count = 0
//                     并撤回待注入的 stall 催促（本轮推进压过"累计 >=3 待催促"）
// 二者直接持有运行时 handler 的 clone（注册时由 running.rs 传入，见
// HookRegister::register_stall_hooks）。

/// 轮开始跟踪：StateChanged{Executing}（仅工具轮）→ stall_count += 1；
/// >= 3 时注入 stall 催促提示词并重置（避免重复催促）。
pub struct StallTracker {
    pub handler: super::AgentHandler,
    pub prompts: Arc<std::sync::Mutex<Vec<PendingPrompt>>>,
    pub nudge: llm::Message,
}

impl Hook for StallTracker {
    fn on_event(&self, ev: &AgentEvent) {
        if let AgentEvent::StateChanged { state } = ev
            && *state == super::AgentState::Executing
            && let Some(plan) = self
                .handler
                .current_plan
                .lock()
                .expect("current_plan lock poisoned")
                .as_mut()
        {
            plan.stall_count += 1;
            if plan.stall_count >= 3 {
                self.prompts
                    .lock()
                    .expect("pending_prompts lock poisoned")
                    .push(PendingPrompt {
                        message: self.nudge.clone(),
                        source: "stall",
                    });
                plan.stall_count = 0;
            }
        }
    }
}

/// CompleteStep 成功重置：stall_count = 0 并撤回 stall 催促
/// （否则连续 2 轮停滞后第 3 轮完成会误触发一次催促）。
pub struct CompleteStepReset {
    pub handler: super::AgentHandler,
    pub prompts: Arc<std::sync::Mutex<Vec<PendingPrompt>>>,
}

impl Hook for CompleteStepReset {
    fn on_event(&self, ev: &AgentEvent) {
        // 仅 CompleteStep 执行成功时重置 stall_count（失败视为停滞更合理）；
        // 撤回 stall 催促无条件执行——但若 current_plan 已被清空（最终
        // CompleteStep 完成），整个守卫不成立，撤回由 unregister_stall_hooks 兜底
        if let AgentEvent::PostToolUse {
            tool_name, result, ..
        } = ev
            && tool_name == "CompleteStep"
            && result.is_ok()
            && let Some(plan) = self
                .handler
                .current_plan
                .lock()
                .expect("current_plan lock poisoned")
                .as_mut()
        {
            plan.stall_count = 0;
            self.prompts
                .lock()
                .expect("pending_prompts lock poisoned")
                .retain(|p| p.source != "stall");
        }
    }
}

// ── stall hooks 的集中注册/注销 ─────────────────────────────────────────────
//
// stall 检测只在 plan 活跃期有意义：CreatePlan 执行后（current_plan 变 Some）
// 由 running.rs 注册，plan 结束（current_plan 清空）后注销。

const STALL_TRACKER_ID: &str = "stall_tracker";
const COMPLETE_STEP_RESET_ID: &str = "complete_step_reset";

impl HookRegister {
    /// 注册 plan 停滞跟踪 hook 对（幂等——已注册则跳过）。`handler` 为当前运行时
    /// 句柄（hook 直接持有其 clone，计划推进/停滞计数写入 current_plan）。
    pub fn register_stall_hooks(&mut self, handler: &super::AgentHandler) {
        if self.has(STALL_TRACKER_ID) {
            return;
        }
        let nudge = llm::Message {
            role: llm::Role::System,
            content: Some(super::prompts::plan::STALL_NUDGE_SUFFIX.to_string()),
            timestamp: Some(chrono::Utc::now()),
            ..Default::default()
        };
        let prompts = self.pending_prompts.clone();
        self.register_with_id(
            STALL_TRACKER_ID,
            StallTracker {
                handler: handler.clone(),
                prompts,
                nudge,
            },
        );
        let prompts = self.pending_prompts.clone();
        self.register_with_id(
            COMPLETE_STEP_RESET_ID,
            CompleteStepReset {
                handler: handler.clone(),
                prompts,
            },
        );
    }

    /// 注销 plan 停滞跟踪 hook 对（幂等）。注销即 plan 结束，同时撤回尚未注入的
    /// stall 催促（覆盖"最终 CompleteStep 清空 current_plan 早于 PostToolUse、
    /// reset hook 无法撤回"的路径，避免残留催促注入到后续轮次）。
    pub fn unregister_stall_hooks(&mut self) {
        self.unregister(STALL_TRACKER_ID);
        self.unregister(COMPLETE_STEP_RESET_ID);
        self.remove_prompts("stall");
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn test_customized_hook_receives_event() {
        let received = Arc::new(Mutex::new(false));
        let received_clone = Arc::clone(&received);

        let hook = CustomizedHook::new(
            AgentEvent::Stop {
                reason: String::new(),
            },
            None,
            Box::new(move |event| {
                if let AgentEvent::Stop { reason } = event {
                    assert_eq!(reason, "done");
                    *received_clone.lock().expect("received_clone lock poisoned") = true;
                }
            }),
        );

        let mut register = HookRegister::new();
        register.register(hook);

        register.emit(&AgentEvent::Stop {
            reason: "done".to_string(),
        });

        assert!(*received.lock().expect("received lock poisoned"));
    }

    #[test]
    fn test_from_str_maps_to_correct_variant() {
        use std::mem::discriminant;

        let cases: &[(&str, AgentEvent)] = &[
            (
                "prompt",
                AgentEvent::UserPromptSubmit {
                    prompt: String::new(),
                },
            ),
            (
                "user_prompt_submit",
                AgentEvent::UserPromptSubmit {
                    prompt: String::new(),
                },
            ),
            (
                "pretool",
                AgentEvent::PreToolUse {
                    tool_name: String::new(),
                    args: serde_json::Value::Null,
                },
            ),
            (
                "pre_tool_use",
                AgentEvent::PreToolUse {
                    tool_name: String::new(),
                    args: serde_json::Value::Null,
                },
            ),
            (
                "posttool",
                AgentEvent::PostToolUse {
                    tool_name: String::new(),
                    result: Ok(ToolResult {
                        output: String::new(),
                        truncated: false,
                    }),
                },
            ),
            (
                "post_tool_use",
                AgentEvent::PostToolUse {
                    tool_name: String::new(),
                    result: Ok(ToolResult {
                        output: String::new(),
                        truncated: false,
                    }),
                },
            ),
            (
                "stop",
                AgentEvent::Stop {
                    reason: String::new(),
                },
            ),
        ];

        for (input, expected) in cases {
            let event: AgentEvent = AgentEvent::from(*input);
            assert_eq!(
                discriminant(&event),
                discriminant(expected),
                "From<&str> for \"{}\" mapped to wrong variant",
                input,
            );
        }
    }

    #[test]
    fn test_customized_hook_filters_by_tool_name() {
        let received = Arc::new(Mutex::new(false));
        let received_clone = Arc::clone(&received);

        let hook = CustomizedHook::new(
            AgentEvent::PreToolUse {
                tool_name: String::new(),
                args: serde_json::Value::Null,
            },
            Some("my_tool".to_string()),
            Box::new(move |_| {
                *received_clone.lock().expect("received_clone lock poisoned") = true;
            }),
        );

        let mut register = HookRegister::new();
        register.register(hook);

        // 不同工具名 → 不应触发
        register.emit(&AgentEvent::PreToolUse {
            tool_name: "other_tool".to_string(),
            args: serde_json::Value::Null,
        });
        assert!(!*received.lock().expect("received lock poisoned"));

        // 匹配工具名 → 应触发
        register.emit(&AgentEvent::PreToolUse {
            tool_name: "my_tool".to_string(),
            args: serde_json::Value::Null,
        });
        assert!(*received.lock().expect("received lock poisoned"));
    }
}
