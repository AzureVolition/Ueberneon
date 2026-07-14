
// ── Tool trait ──────────────────────────────────────────────────────────────

/// 模型可调用的工具 
pub trait ToolMeta: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema，定义工具参数
    fn schema(&self) -> serde_json::Value;
    /// 是否无副作用。Agent 据此决定并行/串行执行。
    fn read_only(&self) -> bool;
}

#[async_trait::async_trait]
pub trait Tool: ToolMeta {
    /// 执行工具，接收模型生成的 raw JSON args，返回文本结果
    async fn execute(&self, ctx: &ToolContext, args: &serde_json::Value) -> ToolResult;
}

// ── AgentMode ──────────────────────────────────────────────────────────────

/// Agent 的全局门控模式，影响权限决策的升降级。
///
/// 模式优先级：谨慎 > 询问 > 自动 > 放飞自我
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentMode {
    /// 谨慎：所有非只读操作都触发询问
    Cautious,
    /// 询问：由各 Check 决定，非交互模式下无 Check 匹配的写操作询问（默认）
    #[default]
    Ask,
    /// 自动：暂未实现，行为等同于 Ask
    Auto,
    /// 放飞自我：从不询问，所有 Ask 退化为 Allow
    Unrestrained,
}

// ── ToolContext ──────────────────────────────────────────────────────────────

/// 工具执行上下文 
pub struct ToolContext {
    /// 工具调用的唯一 ID（stream 中 LLM 返回的 tool_call_id）
    pub call_id: String,
    /// 是否在 plan mode（写工具被阻止）
    pub plan_mode: bool,
    /// Agent 的全局门控模式
    pub agent_mode: AgentMode,
    /// 流式输出回调，长运行工具推送实时输出到前端
    pub progress: Option<Box<dyn Fn(&str) + Send + Sync>>,
}

// ── BlockedKind ──────────────────────────────────────────────────────────────

/// 工具调用被阻止的原因类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedKind {
    /// Plan mode：写工具在 plan mode 被阻止。
    PlanMode,
    /// 权限策略拒绝。
    PermissionDenied,
    /// 文件操作被阻止（如写入已存在的文件）。
    FileBlocked,
    /// 安全限制（如拒绝访问 .git 目录）。
    SecurityRestriction,
}

impl std::fmt::Display for BlockedKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockedKind::PlanMode => write!(f, "plan_mode"),
            BlockedKind::PermissionDenied => write!(f, "permission_denied"),
            BlockedKind::FileBlocked => write!(f, "file_blocked"),
            BlockedKind::SecurityRestriction => write!(f, "security_restriction"),
        }
    }
}

// ── ToolResult ───────────────────────────────────────────────────────────────

/// 工具执行结果。
///
/// 三种变体：
/// - `Success`：正常执行，包含输出文本
/// - `Blocked`：被权限策略或 plan mode 阻止
/// - `Error`：执行出错，错误信息返回给模型
#[derive(Debug, Clone)]
pub enum ToolResult {
    /// 正常执行成功。
    Success {
        /// 返回给模型的文本。
        output: String,
        /// 输出是否被截断（> 32KB）。
        truncated: bool,
    },
    /// 被门禁阻止（plan mode / permission gate）。
    Blocked {
        /// 阻止原因类别。
        kind: BlockedKind,
        /// 模型可见的阻止原因。
        message: String,
    },
    /// 执行出错，错误信息返回给模型。
    Error(String),
}

impl ToolResult {
    /// 是否被门禁阻止
    pub fn is_blocked(&self) -> bool {
        matches!(self, ToolResult::Blocked { .. })
    }

    /// 获取输出文本（Success 返回 output，Blocked 返回 message，Error 返回错误消息）。
    pub fn output(&self) -> &str {
        match self {
            ToolResult::Success { output, .. } => output,
            ToolResult::Blocked { message, .. } => message,
            ToolResult::Error(msg) => msg,
        }
    }

    /// 获取可选错误信息（Success 和 Blocked 返回 None）。
    pub fn error(&self) -> Option<&str> {
        match self {
            ToolResult::Error(msg) => Some(msg),
            _ => None,
        }
    }

    /// 输出是否被截断（仅 Success 有意义，其他变体返回 false）。
    pub fn truncated(&self) -> bool {
        match self {
            ToolResult::Success { truncated, .. } => *truncated,
            _ => false,
        }
    }

    /// 获取阻止原因类别（仅 Blocked 有意义）。
    pub fn blocked_kind(&self) -> Option<BlockedKind> {
        match self {
            ToolResult::Blocked { kind, .. } => Some(*kind),
            _ => None,
        }
    }

    /// 创建成功结果。
    pub fn ok(output: impl Into<String>) -> Self {
        ToolResult::Success {
            output: output.into(),
            truncated: false,
        }
    }

    /// 创建错误结果。
    pub fn err(error: impl Into<String>) -> Self {
        ToolResult::Error(error.into())
    }

    /// 创建被阻止结果。
    pub fn blocked(reason: impl Into<String>) -> Self {
        ToolResult::Blocked {
            kind: BlockedKind::PermissionDenied,
            message: reason.into(),
        }
    }

    /// 设置截断标记（仅对 Success 变体有效）。
    pub fn with_truncated(mut self, val: bool) -> Self {
        match &mut self {
            ToolResult::Success { truncated, .. } => *truncated = val,
            _ => {}
        }
        self
    }
}
