use super::ToolResult;

// ── AgentEvent ───────────────────────────────────────────────────────────────

pub enum AgentEvent {
    /// 用户提交输入后、进入 LLM 前
    UserPromptSubmit {
        prompt: String,
    },
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
    Stop {
        reason: String,
    },
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

pub trait Hook {
    fn on_event(&self, event: &AgentEvent);
}

// ── CustomizedHook ───────────────────────────────────────────────────────────

/// 预置的可自定义 Hook，用户可指定监听的事件种类、工具名，以及回调逻辑。
pub struct CustomizedHook {
    listen_event: AgentEvent,
    listen_tools_name: Option<String>,
    callback: Box<dyn Fn(&AgentEvent)>,
}

impl CustomizedHook {
    pub fn new(
        listen_event: AgentEvent,
        listen_tools_name: Option<String>,
        callback: Box<dyn Fn(&AgentEvent)>,
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

pub struct HookRegister {
    hooks: Vec<Box<dyn Hook>>,
}

impl HookRegister {
    pub fn new() -> Self {
        Self {
            hooks: Vec::new(),
        }
    }

    pub fn register(&mut self, hook: impl Hook + 'static) {
        self.hooks.push(Box::new(hook));
    }

    /// 向所有已注册的 hook 分发事件。
    pub fn emit(&self, event: &AgentEvent) {
        for hook in &self.hooks {
            hook.on_event(event);
        }
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
                    *received_clone.lock().unwrap() = true;
                }
            }),
        );

        let mut register = HookRegister::new();
        register.register(hook);

        register.emit(&AgentEvent::Stop {
            reason: "done".to_string(),
        });

        assert!(*received.lock().unwrap());
    }

    #[test]
    fn test_from_str_maps_to_correct_variant() {
        use std::mem::discriminant;

        let cases: &[(&str, AgentEvent)] = &[
            ("prompt", AgentEvent::UserPromptSubmit { prompt: String::new() }),
            ("user_prompt_submit", AgentEvent::UserPromptSubmit { prompt: String::new() }),
            ("pretool", AgentEvent::PreToolUse { tool_name: String::new(), args: serde_json::Value::Null }),
            ("pre_tool_use", AgentEvent::PreToolUse { tool_name: String::new(), args: serde_json::Value::Null }),
            ("posttool", AgentEvent::PostToolUse {
                tool_name: String::new(),
                result: Ok(ToolResult { output: String::new(), truncated: false }),
            }),
            ("post_tool_use", AgentEvent::PostToolUse {
                tool_name: String::new(),
                result: Ok(ToolResult { output: String::new(), truncated: false }),
            }),
            ("stop", AgentEvent::Stop { reason: String::new() }),
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
                *received_clone.lock().unwrap() = true;
            }),
        );

        let mut register = HookRegister::new();
        register.register(hook);

        // 不同工具名 → 不应触发
        register.emit(&AgentEvent::PreToolUse {
            tool_name: "other_tool".to_string(),
            args: serde_json::Value::Null,
        });
        assert!(!*received.lock().unwrap());

        // 匹配工具名 → 应触发
        register.emit(&AgentEvent::PreToolUse {
            tool_name: "my_tool".to_string(),
            args: serde_json::Value::Null,
        });
        assert!(*received.lock().unwrap());
    }
}
