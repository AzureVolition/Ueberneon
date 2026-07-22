pub mod hook;
pub mod main_agent;
pub mod action_plan;
pub mod manager;
pub mod prompts;
use anyhow::Context;
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

/// 执行上下文 
pub struct ToolContext {
    /// 工具调用的唯一 ID（stream 中 LLM 返回的 tool_call_id）
    pub call_id: String,
    /// 计划模式（常规/计划），写工具在计划模式被阻止
    pub plan_mode: ActionMode,
    /// 流式输出回调，长运行工具推送实时输出到前端
    pub progress: Option<Box<dyn Fn(&str) + Send + Sync>>,
    /// 运行时控制句柄（与 agent_mode 指向同一 Arc）
    pub handler: AgentHandler,
    /// 主 Agent 的 conversation_id，用于子 Agent 设置 parent_conversation_id
    pub main_conversation_id: String,
    /// 项目 ID
    pub project_id: Option<String>,
    /// 取消令牌，工具可监听以实现提前终止
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
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
use std::sync::{Arc, Mutex, RwLock};
use crate::tools::Registry;
use crate::model::{ChatMessage, Plan, PlanStatus, StepStatus, StreamSegment, ToolCallRecord, ToolCallStatus};
use hook::HookRegister;
use llm::{Message, Provider, Role as LlmRole};

/// Agent 运行时控制句柄，前端持有以实时调整 Agent 行为。
#[derive(Clone)]
pub struct AgentHandler {
    /// 全局门控模式（Arc 共享，供 handler 和内部读取）
    pub agent_mode: Arc<Mutex<AgentMode>>,
    /// 计划模式（Arc 共享，供前端切换和内部读取）
    pub action_mode: Arc<RwLock<ActionMode>>,
    /// 当前计划数据（Arc 共享，供 UI 读取）
    pub current_plan: Arc<Mutex<Option<Plan>>>,
}

pub enum CurrentPlanState {
    Init,
    Debate,
    Exculuding,
}

impl AgentHandler {
    /// 从父 handler 继承运行时状态（agent_mode + action_mode）。
    /// 子 agent 创建后调用此方法，将父 handler 的状态同步过来。
    pub fn inherit_from(&mut self, parent: &AgentHandler) {
        let mode = *parent.agent_mode.lock().expect("agent_mode lock poisoned");
        *self.agent_mode.lock().expect("agent_mode lock poisoned") = mode;
        let am = *parent.action_mode.read().expect("action_mode lock poisoned");
        *self.action_mode.write().expect("action_mode lock poisoned") = am;
    }

    /// 读取当前 action_mode。
    pub fn action_mode(&self) -> ActionMode {
        *self.action_mode.read().expect("action_mode lock poisoned")
    }

    /// 设置 action_mode（前端/测试使用）。
    pub fn set_action_mode(&self, mode: ActionMode) {
        *self.action_mode.write().expect("action_mode lock poisoned") = mode;
    }

    /// 创建默认的 AgentHandler（用于测试）。
    pub fn default() -> Self {
        Self {
            agent_mode: Arc::new(Mutex::new(AgentMode::default())),
            action_mode: Arc::new(RwLock::new(ActionMode::default())),
            current_plan: Arc::new(Mutex::new(None)),
        }
    }

    pub fn current_plan_state(&self) -> Option<CurrentPlanState> {
        let guard = self.current_plan.lock().expect("current_plan lock poisoned").clone();
        let action_mode_guard = self.action_mode.read().expect("action_mode lock poisoned");
        if guard.is_none() && *action_mode_guard == ActionMode::Regular {
            return None;
        }
        if guard.is_none() && *action_mode_guard == ActionMode::Plan {
            return Some(CurrentPlanState::Init);
        }
        if PlanStatus::NeedApproval == guard?.status {
            return Some(CurrentPlanState::Debate);
        }
    
        Some(CurrentPlanState::Exculuding)
        
    }

    pub fn prompt_before_user_message(&self) -> Option<&str> {
        if let Some(action_mode) = self.current_plan_state() {
            match action_mode {
                CurrentPlanState::Init => {
                    Some(prompts::plan::PLAN_CREATE_PREFIX)
                }
                CurrentPlanState::Debate => {
                    Some(prompts::plan::PLAN_MODIFY_PREFIX)
                }
                _ => {
                    None
                }
            }
        } else {
            None
        }
    }

    pub fn prompt_pre_loop(&self) -> Option<&str> {
        None
    }

    /// 审批通过当前计划：写入 DB（plan + tasks），NeedApproval → InProgress，
    /// 第一个 Pending step → InProgress，action_mode → Regular。
    pub fn approve_plan(&self, project_id: &str, conversation_id: &str) -> Result<(), String> {
        let plan_clone;
        {
            let mut guard = self.current_plan.lock().expect("current_plan lock poisoned");
            let plan = match guard.as_mut() {
                Some(p) => p,
                None => return Err("no active plan to approve".to_string()),
            };

            if plan.status != PlanStatus::NeedApproval {
                return Err("plan is not in NeedApproval status".to_string());
            }

            plan.status = PlanStatus::InProgress;
            plan.started_at = Some(chrono::Utc::now());

            // 第一个 Pending step → InProgress
            for step in plan.steps.iter_mut() {
                if step.status == StepStatus::Pending {
                    step.status = StepStatus::InProgress;
                    break;
                }
            }

            plan_clone = plan.clone();

            // 切换 action_mode
            *self.action_mode.write().expect("action_mode lock poisoned") = ActionMode::Regular;
        }

        // ── 写入数据库（此时才真正入库）──
        crate::db::with_db_result(|conn| {
            use crate::db::metadata::plan::{self as plan_db, PlanStatus as DbPlanStatus};
            let plan_id = plan_db::create(
                conn,
                project_id,
                conversation_id,
                &plan_clone.goal,
                &plan_clone.description,
                DbPlanStatus::InProgress,
            )
            .map_err(|e| format!("db error: {e}"))?;

            // 记录开始时间
            plan_db::mark_started(conn, &plan_id)
                .map_err(|e| format!("db error: {e}"))?;

            // 递归写入 tasks
            fn persist_step(
                conn: &rusqlite::Connection,
                plan_id: &str,
                project_id: &str,
                parent_id: Option<i64>,
                step: &crate::model::ActionStep,
            ) -> Result<(), String> {
                use crate::db::metadata::task::{self as task_db, TaskStatus as DbTaskStatus};

                let db_status: DbTaskStatus = step.status.clone().into();
                let task_id = task_db::create(
                    conn, plan_id, project_id, parent_id,
                    step.index as i32, &step.description, db_status,
                    None,
                )
                .map_err(|e| format!("db error: {e}"))?;

                if let Some(children) = &step.children {
                    for child in children {
                        persist_step(conn, plan_id, project_id, Some(task_id), child)?;
                    }
                }
                Ok(())
            }

            for step in &plan_clone.steps {
                persist_step(conn, &plan_id, project_id, None, step)?;
            }

            Ok::<_, String>(())
        })?;

        Ok(())
    }

    /// 拒绝当前计划：清除内存中的 current_plan（不入库）。
    pub fn reject_plan(&self) -> Result<(), String> {
        let guard = self.current_plan.lock().expect("current_plan lock poisoned");
        if guard.is_none() {
            return Err("no active plan to reject".to_string());
        }
        Ok(())
    }
}

/// Agent —— 拥有 provider 和 registry，通过 mpsc channel 输出流式事件。
/// 自己管理消息历史 + 本地持久化，与 UI 层解耦。
pub struct Agent {
    /// LLM provider（所有权）
    pub provider: Box<dyn Provider>,
    /// 工具注册表（所有权）
    pub registry: Registry,
    /// 事件钩子注册表
    pub hook_register: HookRegister,
    /// 运行时控制句柄（含 action_mode / agent_mode / current_plan）
    pub handler: AgentHandler,
    /// 工具执行的工作目录（即项目路径）
    pub project_path: PathBuf,
    /// 项目 ID（用于持久化）
    pub project_id: Option<String>,
    /// 对话 ID（用于持久化）
    pub conversation_id: String,
    /// LLM 消息历史（Agent 自己管理）
    pub messages: Vec<Message>,
    /// 内部流式状态（由 create_streaming() 初始化）
    pub streaming_handle: Option<crate::model::StreamingState>,
    /// 推理温度
    pub temperature: f64,
    /// 最大 token 数
    pub max_tokens: Option<u32>,
    /// Agent 类型
    pub agent_type: String,
}

impl Agent {
    /// 创建 Agent，获得 provider 和 registry 的所有权。
    pub fn new(
        provider: Box<dyn Provider>,
        registry: Registry,
        hook_register: HookRegister,
        project_path: PathBuf,
        project_id: Option<String>,
        conversation_id: String,
        temperature: f64,
        max_tokens: Option<u32>,
        agent_type: String,
    ) -> Self {
        let handler = AgentHandler {
            agent_mode: Arc::new(Mutex::new(AgentMode::default())),
            action_mode: Arc::new(RwLock::new(ActionMode::default())),
            current_plan: Arc::new(Mutex::new(None)),
        };        
        Self {
            provider,
            registry,
            hook_register,
            handler,
            project_path,
            project_id,
            conversation_id,
            messages: Vec::new(),
            streaming_handle: None,
            temperature,
            max_tokens,
            agent_type,
        }
    }

    pub fn push_message(&mut self, msg: Message) -> anyhow::Result<()> {
        if let Ok(guard) = crate::db::get_db().lock() {
            self.save_message(&guard, &msg).context("save message")?;
            self.touch_conversation(&guard).context("touch conversation")?;
        }
        self.messages.push(msg);
        Ok(())
    }

    /// 初始化消息历史：写入 system prompt，清空旧历史。
    pub fn init_history(&mut self, system_prompt: String) {
        self.messages.clear();
        self.messages.push(Message {
            role: LlmRole::System,
            content: Some(system_prompt),
            ..Default::default()
        });
    }

    /// 从 UI 层消息加载对话历史（不含当前用户输入）。
    /// 调用者应确保传入的消息不包含未处理的用户输入。
    pub fn load_history(&mut self, chat_messages: &[ChatMessage]) {
        for m in chat_messages {
            self.messages.push(Message {
                role: match m.role {
                    crate::model::Role::User => LlmRole::User,
                    crate::model::Role::Assistant => LlmRole::Assistant,
                    crate::model::Role::System => LlmRole::System,
                },
                content: Some(m.content.clone()),
                ..Default::default()
            });
        }
    }

    /// 从 LLM 消息历史导出 ChatMessage 列表（供 UI 显示）。
    /// 不含 segments/tool_result，使用纯文本 content + reasoning。
    pub fn chat_messages(&self) -> Vec<ChatMessage> {
        let mut result = Vec::new();
        for m in &self.messages {
            match m.role {
                LlmRole::User => {
                    result.push(ChatMessage {
                        role: crate::model::Role::User,
                        content: m.content.clone().unwrap_or_default(),
                        timestamp: chrono::Local::now(),
                        tool_calls: Vec::new(),
                        reasoning: String::new(),
                        segments: Vec::new(),
                        content_html: String::new(),
                    });
                }
                LlmRole::Assistant => {
                    let content = m.content.clone().unwrap_or_default();
                    let tcs: Vec<ToolCallRecord> = m.tool_calls.iter().map(|tc| ToolCallRecord {
                        tool_name: tc.name.clone(),
                        args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                        result: None,
                        status: ToolCallStatus::Success,
                        approval_reason: None,
                    }).collect();
                    // 构建 segments：reasoning → text → tool call markers
                    let mut segs: Vec<StreamSegment> = Vec::new();
                    let reasoning_text = m.reasoning_content.clone().unwrap_or_default();
                    if !reasoning_text.is_empty() {
                        segs.push(StreamSegment::Reasoning(reasoning_text.clone()));
                    }
                    if !content.is_empty() {
                        segs.push(StreamSegment::Text(content.clone()));
                    }
                    for _ in 0..tcs.len() {
                        segs.push(StreamSegment::ToolCall);
                    }
                    result.push(ChatMessage {
                        role: crate::model::Role::Assistant,
                        content,
                        timestamp: chrono::Local::now(),
                        tool_calls: tcs,
                        reasoning: reasoning_text,
                        segments: segs,
                        content_html: String::new(),
                    });
                }
                _ => {}
            }
        }
        result
    }
}
