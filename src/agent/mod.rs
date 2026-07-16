pub mod hook;
pub mod main_agent;
pub mod action_plan;
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


// ── PlanMode ──────────────────────────────────────────────────────────────

/// Plan mode 枚举：控制工具在计划阶段的可执行性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActionMode {
    /// 常规模式：所有工具均可正常执行
    #[default]
    Regular,
    /// 计划模式：仅只读工具可执行，写工具被阻止
    Plan,
}

impl ActionMode {
    /// 用于 HTML option value 的键。
    pub fn as_key(self) -> &'static str {
        match self {
            ActionMode::Regular => "regular",
            ActionMode::Plan => "plan",
        }
    }

    /// 所有变体，供 UI 遍历。
    pub const ALL: &[ActionMode] = &[ActionMode::Regular, ActionMode::Plan];
}

impl std::fmt::Display for ActionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ActionMode::Regular => write!(f, "常规"),
            ActionMode::Plan => write!(f, "计划"),
        }
    }
}

impl std::str::FromStr for ActionMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "regular" => Ok(ActionMode::Regular),
            "plan" => Ok(ActionMode::Plan),
            _ => Err(format!("unknown ActionMode key: {s}")),
        }
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

impl AgentMode {
    /// 用于 HTML option value 的键。
    pub fn as_key(self) -> &'static str {
        match self {
            AgentMode::Cautious => "cautious",
            AgentMode::Ask => "ask",
            AgentMode::Auto => "auto",
            AgentMode::Unrestrained => "unrestrained",
        }
    }

    /// 所有变体，供 UI 遍历。
    pub const ALL: &[AgentMode] = &[
        AgentMode::Cautious,
        AgentMode::Ask,
        AgentMode::Auto,
        AgentMode::Unrestrained,
    ];
}

impl std::fmt::Display for AgentMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentMode::Cautious => write!(f, "谨慎"),
            AgentMode::Ask => write!(f, "询问"),
            AgentMode::Auto => write!(f, "自动"),
            AgentMode::Unrestrained => write!(f, "放飞自我"),
        }
    }
}

impl std::str::FromStr for AgentMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "cautious" => Ok(AgentMode::Cautious),
            "ask" => Ok(AgentMode::Ask),
            "auto" => Ok(AgentMode::Auto),
            "unrestrained" => Ok(AgentMode::Unrestrained),
            _ => Err(format!("unknown AgentMode key: {s}")),
        }
    }
}

// ── ToolContext ──────────────────────────────────────────────────────────────

/// 工具执行上下文 
pub struct ToolContext {
    /// 工具调用的唯一 ID（stream 中 LLM 返回的 tool_call_id）
    pub call_id: String,
    /// 计划模式（常规/计划），写工具在计划模式被阻止
    pub plan_mode: ActionMode,
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

use std::path::PathBuf;
use crate::tools::Registry;
use hook::HookRegister;
use llm::Provider;

/// Agent —— 拥有 provider 和 registry，通过 mpsc channel 输出流式事件。
pub struct Agent {
    /// LLM provider（所有权）
    pub provider: Box<dyn Provider>,
    /// 工具注册表（所有权）
    pub registry: Registry,
    /// 事件钩子注册表
    pub hook_register: HookRegister,
    /// 计划模式（常规/计划）
    pub plan_mode: ActionMode,
    /// 全局门控模式
    pub agent_mode: AgentMode,
    /// 工具执行的工作目录（即项目路径）
    pub project_path: PathBuf,
}
