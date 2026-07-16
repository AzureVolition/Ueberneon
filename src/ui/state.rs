use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

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
}

/// 单次工具调用记录
#[derive(Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result: Option<String>,
    pub status: ToolCallStatus,
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
}

/// 对话
#[derive(Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
}

/// 项目
#[derive(Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: DateTime<Local>,
    pub conversations: Vec<Conversation>,
}

/// 侧边栏视图状态
#[derive(Clone, PartialEq)]
pub enum SidebarView {
    /// 显示项目列表
    ProjectList,
    /// 显示指定项目的对话列表
    ConversationList(String),
}

/// 应用配置
#[derive(Clone)]
pub struct AppConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub agent_mode: String,
}
