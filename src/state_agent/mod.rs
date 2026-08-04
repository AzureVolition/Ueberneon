pub mod hook;
pub mod main_agent;
pub mod action_plan;
pub mod manager;
pub mod prompts;
pub mod agent_run;
pub mod approval;
pub mod state_agent;
pub mod running;

use anyhow::Context;
pub use agent_run::{AgentRun, AgentState, Blocked, PendingApproval, PendingResume, StopReason};
pub use approval::{ApprovalChain, ApprovalCtx, ApprovalGate, UserApprovalGate};
pub use llm::{tool::ToolMeta, ToolCall};

// ── Tool trait ──────────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait Tool: ToolMeta {
    /// 执行工具，接收模型生成的 raw JSON args
    async fn execute(&self, ctx: &ToolContext, args: &serde_json::Value) -> Result<ToolResult, String>;
}

#[async_trait::async_trait]
pub trait GenericsType: ToolMeta + Send + Sync 
{
    type ArgType: serde::de::DeserializeOwned + Send + Sync;
}

#[async_trait::async_trait]
pub trait GenericsTool: GenericsType
{
    async fn generics_execute(&self, ctx: &ToolContext, args: &Self::ArgType) -> Result<ToolResult, String>;
}

#[async_trait::async_trait]
impl <G> Tool for G
    where  G: GenericsTool + Send + Sync ,
{
    async fn execute(&self, ctx: &ToolContext, args: &serde_json::Value) -> Result<ToolResult, String> {
        let params: G::ArgType = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => return Err(format!("bash: invalid arguments: {e}")),
        };
        self.generics_execute(ctx, &params).await
    }
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
    /// 计划版本号（CompleteStep 成功后递增，UI 依赖此值刷新）
    pub plan_version: Arc<std::sync::atomic::AtomicU64>,
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
            plan_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
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

    /// 可以抽象出来的方法,未来可以弄成抽象Handler用以塞入提示词
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

    pub fn prompt_pre_loop(&self) -> Option<String> {
        if let Some(action_mode) = self.current_plan_state() {
            match action_mode {
                CurrentPlanState::Exculuding => {
                    let plan = self.current_plan.lock().expect("current_plan lock poisoned").clone()?;
                    Some(crate::agent::prompts::plan::execute_prompt(&plan))
                }
                _ => None,
            }
        } else {
            None
        }
    }

    pub fn can_finish(&self) -> Option<String> {
        if let Some(plan) = self.current_plan.lock().expect("current_plan lock poisoned").as_ref() && plan.status == PlanStatus::InProgress {
            Some("current plan is not finished".to_string())
        }else {
            None
        }
        
    }

    /// 审批通过当前计划：写入 DB（plan + tasks），构建 completion_queue，
    /// 第一个 Pending batch → Current，action_mode → Regular。
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

            plan_clone = plan.clone();

            // 切换 action_mode
            *self.action_mode.write().expect("action_mode lock poisoned") = ActionMode::Regular;
        }

        // ── 写入数据库 ──
        use crate::model::QueueItemStatus;
        use crate::db::metadata::plan::{self as plan_db, PlanStatus as DbPlanStatus};

        let plan_id = crate::db::with_db_result(|conn| {
            let pid = plan_db::create(
                conn, project_id, conversation_id,
                &plan_clone.goal, &plan_clone.description,
                DbPlanStatus::InProgress,
            ).map_err(|e| format!("db error: {e}"))?;

            plan_db::mark_started(conn, &pid)
                .map_err(|e| format!("db error: {e}"))?;

            // 递归展平树写入 DB，同时构建队列
            let mut queue: Vec<crate::model::QueueItem> = Vec::new();
            flatten_and_write(&plan_clone.children, conn, &pid, project_id, None, None, &mut queue, 0)
                .map_err(|e| format!("db error: {e}"))?;

            // 把队列写回 plan（通过 map，后面会 clone 出去）
            // 这里不能直接修改 plan，因为 conn 的闭包中不能持有 guard
            // 返回 queue 和 pid，后面再写回
            Ok::<_, String>((pid, queue))
        }).map_err(|e| format!("db error: {e}"))?;

        let (plan_id, mut queue) = plan_id;

        // 设置第一个队列项为 Current
        if let Some(first) = queue.first_mut() {
            first.status = QueueItemStatus::Current;
            if let Some(entity) = first.batch.first_mut() {
                entity.step_status = StepStatus::InProgress;
            }
        }

        // 把数据库 plan_id 和队列写回内存中的 plan
        {
            let mut guard = self.current_plan.lock().expect("current_plan lock poisoned");
            if let Some(ref mut p) = guard.as_mut() {
                p.db_plan_id = Some(plan_id);
                p.completion_queue = queue;
                p.children.clear(); // 树已转为队列，清空暂存
            }
        }

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

/// 递归展平 PlanNode 树写入 tasks 表，同时构建 completion_queue
fn flatten_and_write(
    nodes: &[crate::model::PlanNode],
    conn: &rusqlite::Connection,
    plan_id: &str,
    project_id: &str,
    parent_db_id: Option<i64>,
    parent_node_idx: Option<u8>,
    queue: &mut Vec<crate::model::QueueItem>,
    depth: u8,
) -> Result<(), String> {
    use crate::db::metadata::task::{self as task_db, TaskStatus as DbTaskStatus};
    use crate::model::{Entity, QueueItem, QueueItemStatus, StepStatus};

    let mut sorted = nodes.to_vec();
    sorted.sort_by_key(|n| n.idx);

    for node in &sorted {
        // 写入当前节点到 DB
        let store_idx: i32 = if depth == 0 && !node.children.is_empty() {
            -1 // phase 节点存 -1
        } else {
            node.idx as i32
        };
        let task_id = task_db::create(
            conn, plan_id, project_id, parent_db_id,
            store_idx, &node.description, DbTaskStatus::Pending, None,
        ).map_err(|e| format!("{}", e))?;

        if node.children.is_empty() {
            // 叶子节点 → 创建队列批次
            let entity = Entity {
                db_id: Some(task_id),
                idx: node.idx,
                parent_idx: parent_node_idx,
                description: node.description.clone(),
                step_status: StepStatus::Pending,
            };
            queue.push(QueueItem {
                status: QueueItemStatus::Pending,
                batch: vec![entity],
            });
        } else {
            // 非叶子节点 → 递归子节点
            flatten_and_write(
                &node.children, conn, plan_id, project_id,
                Some(task_id), Some(node.idx), queue, depth + 1,
            )?;

            // 最后一个子节点的 batch 追加父节点
            if let Some(last_qi) = queue.last_mut() {
                last_qi.batch.push(Entity {
                    db_id: Some(task_id),
                    idx: node.idx,
                    parent_idx: None,
                    description: node.description.clone(),
                    step_status: StepStatus::Pending,
                });
            }
        }
    }

    Ok(())
}

/// Agent —— 拥有 provider 和 registry，通过 mpsc channel 输出流式事件。
/// 自己管理消息历史 + 本地持久化，与 UI 层解耦。
pub struct Agent {
    /// LLM provider（所有权）
    pub provider: Box<dyn Provider>,
    /// 工具注册表
    pub registry: Arc<Registry>,
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
    /// 推理温度
    pub temperature: f64,
    /// 最大 token 数
    pub max_tokens: Option<u32>,
    /// 上下文窗口上限
    pub context_window: u32,
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
        context_window: u32,
        agent_type: String,
    ) -> Self {
        let handler = AgentHandler {
            agent_mode: Arc::new(Mutex::new(AgentMode::default())),
            action_mode: Arc::new(RwLock::new(ActionMode::default())),
            current_plan: Arc::new(Mutex::new(None)),
            plan_version: Arc::new(std::sync::atomic::AtomicU64::new(0)),
        };        
        Self {
            provider,
            registry: Arc::new(registry),
            hook_register,
            handler,
            project_path,
            project_id,
            conversation_id,
            messages: Vec::new(),
            temperature,
            max_tokens,
            context_window,
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
                        reasoning: String::new(),
                        segments: Vec::new(),
                        content_html: String::new(),
                    });
                }
                LlmRole::Assistant => {
                    let content = m.content.clone().unwrap_or_default();
                    // 构建 segments：reasoning → text → tool calls（记录内嵌）
                    let mut segs: Vec<StreamSegment> = Vec::new();
                    let reasoning_text = m.reasoning_content.clone().unwrap_or_default();
                    if !reasoning_text.is_empty() {
                        segs.push(StreamSegment::Reasoning(reasoning_text.clone()));
                    }
                    if !content.is_empty() {
                        segs.push(StreamSegment::Text(content.clone()));
                    }
                    for tc in &m.tool_calls {
                        segs.push(StreamSegment::ToolCall(ToolCallRecord {
                            tool_name: tc.name.clone(),
                            args: serde_json::from_str(&tc.arguments).unwrap_or_default(),
                            result: None,
                            status: ToolCallStatus::Success,
                            approval_reason: None,
                        }));
                    }
                    result.push(ChatMessage {
                        role: crate::model::Role::Assistant,
                        content,
                        timestamp: chrono::Local::now(),
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


pub enum InterruptState {
    Cancelled,
    Error(String),
}

pub struct Agent<T> {

    pub running: T,
    /// LLM provider（所有权）
    pub provider: Box<dyn Provider>,
    /// 工具注册表
    //pub registry: Arc<Registry>,
    pub tools: Vec<String>,

    /// 工具执行的工作目录（即项目路径）
    pub project_path: PathBuf,
    /// 项目 ID（用于持久化）
    pub project_id: Option<String>,
    /// 对话 ID（用于持久化）
    pub conversation_id: String,
    /// LLM 消息历史（Agent 自己管理）
    pub messages: Vec<Message>,
    /// 推理温度
    pub temperature: f64,
    /// 最大 token 数
    pub max_tokens: u32,
    /// 上下文窗口上限
    pub context_window: u32,
    /// Agent 类型
    pub agent_type: String,
}

impl Agent<Static> {
    pub fn accept_message(&mut self, user_input: String) -> Result<Agent<Running<Streaming>>, InterruptState> {

        
        let req = Request {
            messages: self.agent.messages.clone(),
            tools: self.agent.registry.schemas(),
            temperature: self.agent.temperature,
            max_tokens: self.agent.max_tokens.unwrap_or(65536),
        };


        let (run, rx) = Running<Streaming>::init(&req);
        let running_agent = Agent {
            running: run,
            provider: self.provider,
            tools: self.tools,
            project_path: self.project_path,
            project_id: self.project_id,
            conversation_id: self.conversation_id,
            messages: self.messages,
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            context_window: self.context_window,
            agent_type: self.agent_type,
        };

        Ok(running_agent)
    }
}