// ── 核心数据模型 ──
//
// 独立于 UI 层，供 agent、store、ui 等模块共享。
// 从 src/ui/state.rs 迁移而来。

use chrono::{DateTime, Local, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::sync::atomic::AtomicU64;

/// 消息角色
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 工具调用状态
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Running,
    Success,
    Failed(String),
    /// 被权限策略或用户拒绝
    Denied(String),
    /// 等待用户审批
    AwaitingApproval {
        reason: String,
    },
}

/// 单次工具调用记录
#[derive(Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result: Option<String>,
    pub status: ToolCallStatus,
    /// 审批原因（仅 AwaitingApproval 时填充）
    #[serde(default)]
    pub approval_reason: Option<String>,
}

/// 一条聊天消息
#[derive(Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Local>,
    pub tool_calls: Vec<ToolCallRecord>,
    /// LLM 的推理/思考内容（渲染为可折叠区域）
    #[serde(default)]
    pub reasoning: String,
    /// 按 LLM 返回顺序的流式片段（用于有序渲染）
    #[serde(default)]
    pub segments: Vec<StreamSegment>,
    /// 预渲染的 content HTML（加载时计算，避免每次渲染重复解析 markdown）
    #[serde(skip)]
    pub content_html: String,
}

/// 流式输出片段 —— 按 LLM 返回顺序排列，Frontend 依此渲染
#[derive(Clone, Serialize, Deserialize)]
pub enum StreamSegment {
    /// 文本片段
    Text(String),
    /// 推理/思考片段
    Reasoning(String),
    /// 工具调用插入点（调用详情从 ToolCallRecord 获取）
    ToolCall,
}

/// 对话
#[derive(Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    /// 最后活动时间
    #[serde(default)]
    pub updated_at: DateTime<Local>,
    /// 消息总数（从 DB 查询，序列化忽略）
    #[serde(default)]
    pub message_count: usize,
}

/// 项目
#[derive(Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: DateTime<Local>,
    pub conversations: Vec<Conversation>,
    /// 自定义 indicator 颜色键（""=默认 cyan）
    #[serde(default)]
    pub indicator_color: String,
    /// 项目最近活跃时间（删对话也不丢失）
    #[serde(default)]
    pub last_activity_at: Option<DateTime<Local>>,
}

/// 从消息中提取对话标题（用首条用户消息的前 N 个字符）
pub fn title_from_messages(messages: &[ChatMessage]) -> String {
    for msg in messages {
        if matches!(msg.role, Role::User) {
            let trimmed = msg.content.trim();
            if !trimmed.is_empty() {
                let max_len = 30;
                if trimmed.len() <= max_len {
                    return trimmed.to_string();
                }
                // 按字符边界截断（UTF-8 安全）
                let mut truncated = String::with_capacity(max_len);
                for ch in trimmed.chars() {
                    if truncated.len() + ch.len_utf8() > max_len {
                        break;
                    }
                    truncated.push(ch);
                }
                truncated.push('…');
                return truncated;
            }
        }
    }
    "new conversation".into()
}

/// 计算对话的轮数（user + assistant 消息对数）
pub fn conversation_rounds(messages: &[ChatMessage]) -> usize {
    let user_count = messages.iter().filter(|m| matches!(m.role, Role::User)).count();
    let assistant_count = messages.iter().filter(|m| matches!(m.role, Role::Assistant)).count();
    user_count.min(assistant_count)
}

/// 格式化相对时间（如 "3m ago", "2h ago", "1d ago"）
pub fn format_relative_time(dt: &DateTime<Local>) -> String {
    let now = Local::now();
    let diff = *dt - now;
    let secs = -diff.num_seconds();

    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 2592000 {
        format!("{}d ago", secs / 86400)
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

// ── Plan / ActionStep types ──────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct Plan {
    pub goal: String,
    pub description: String,
    pub steps: Vec<ActionStep>,
    #[serde(default)]
    pub status: PlanStatus,
    pub started_at: Option<DateTime<Utc>>,
    /// 连续未完成步骤的轮次计数（≥3 时注入催促提示）
    #[serde(default)]
    pub stall_count: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct ActionStep {
    pub index: u8,
    #[serde(default)]
    pub status: StepStatus,
    pub description: String,
    #[serde(default)]
    pub children: Option<Vec<ActionStep>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub enum StepStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Bolcked,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub enum PlanStatus {
    #[default]
    NeedApproval,
    InProgress,
    Completed,
}

// ── 与 db 层的枚举互转 ──

use crate::db::metadata::plan::PlanStatus as DbPlanStatus;
use crate::db::metadata::task::TaskStatus as DbTaskStatus;

impl From<PlanStatus> for DbPlanStatus {
    fn from(s: PlanStatus) -> Self {
        match s {
            PlanStatus::NeedApproval => DbPlanStatus::NeedApproval,
            PlanStatus::InProgress => DbPlanStatus::InProgress,
            PlanStatus::Completed => DbPlanStatus::Completed,
        }
    }
}

impl From<DbPlanStatus> for PlanStatus {
    fn from(s: DbPlanStatus) -> Self {
        match s {
            DbPlanStatus::NeedApproval => PlanStatus::NeedApproval,
            DbPlanStatus::InProgress => PlanStatus::InProgress,
            DbPlanStatus::Completed => PlanStatus::Completed,
            DbPlanStatus::Canceled => PlanStatus::InProgress, // 历史数据兼容：Canceled 视为可重新执行
        }
    }
}

impl From<StepStatus> for DbTaskStatus {
    fn from(s: StepStatus) -> Self {
        match s {
            StepStatus::Pending => DbTaskStatus::Pending,
            StepStatus::InProgress => DbTaskStatus::InProgress,
            StepStatus::Completed => DbTaskStatus::Completed,
            StepStatus::Bolcked => DbTaskStatus::Blocked,
            StepStatus::Failed => DbTaskStatus::Failed,
        }
    }
}

impl From<DbTaskStatus> for StepStatus {
    fn from(s: DbTaskStatus) -> Self {
        match s {
            DbTaskStatus::Pending => StepStatus::Pending,
            DbTaskStatus::InProgress => StepStatus::InProgress,
            DbTaskStatus::Completed => StepStatus::Completed,
            DbTaskStatus::Blocked => StepStatus::Bolcked,
            DbTaskStatus::Failed => StepStatus::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema,PartialEq)]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
}

// ── UI 消息 ──────────────────────────────────────────────────────────────────

/// Agent 内部流式状态，通过 Arc 与 UI 共享
#[derive(Clone)]
pub struct StreamingState {
    pub segments: Arc<Mutex<Vec<StreamSegment>>>,
    pub tool_calls: Arc<Mutex<Vec<ToolCallRecord>>>,
    pub version: Arc<AtomicU64>,
    pub approval_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
}

/// UI 层的消息表示。运行时使用，不持久化。
#[derive(Clone)]
pub enum UiMessage {
    /// 已完成的静态消息
    Static(ChatMessage),
    /// 流式进行中的消息：segments 和 tool_calls 由 Agent 异步填充，
    /// version 用于触发 Dioxus 重渲染（每次 push 后递增）。
    Streaming {
        segments: Arc<Mutex<Vec<StreamSegment>>>,
        tool_calls: Arc<Mutex<Vec<ToolCallRecord>>>,
        version: Arc<AtomicU64>,
        approval_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
    },
}
