pub mod hook;
pub use llm::tool::ToolMeta;

// ── Tool trait ──────────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait Tool: ToolMeta {
    /// 执行工具，接收模型生成的 raw JSON args
    async fn execute(&self, ctx: &ToolContext, args: &serde_json::Value) -> Result<ToolResult, String>;
}

// ── ToolResult ───────────────────────────────────────────────────────────────

/// 工具执行成功结果。
#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 返回给模型的文本。
    pub output: String,
    /// 输出是否被截断（> 32KB）。
    pub truncated: bool,
}

impl ToolResult {
    /// 创建成功结果。
    pub fn ok(output: impl Into<String>) -> Self {
        ToolResult {
            output: output.into(),
            truncated: false,
        }
    }

    /// 设置截断标记。
    pub fn with_truncated(mut self, val: bool) -> Self {
        self.truncated = val;
        self
    }
}

/// 为 `Result<ToolResult, String>` 提供兼容访问器。
pub trait ToolResultExt {
    fn output(&self) -> &str;
    fn error(&self) -> Option<&str>;
    fn truncated(&self) -> bool;
    fn is_blocked(&self) -> bool;
}

impl ToolResultExt for Result<ToolResult, String> {
    fn output(&self) -> &str {
        match self {
            Ok(tr) => &tr.output,
            Err(msg) => msg,
        }
    }

    fn error(&self) -> Option<&str> {
        match self {
            Ok(_) => None,
            Err(msg) => Some(msg),
        }
    }

    fn truncated(&self) -> bool {
        match self {
            Ok(tr) => tr.truncated,
            Err(_) => false,
        }
    }

    fn is_blocked(&self) -> bool {
        self.is_err()
    }
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

// —— agent ————————————————————————————————————————————————————————————————————

pub struct Agent {
    
}
