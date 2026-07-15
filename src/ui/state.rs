use chrono::{DateTime, Local};

/// 消息角色
#[derive(Clone, PartialEq)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 工具调用状态
#[derive(Clone, PartialEq)]
pub enum ToolCallStatus {
    Running,
    Success,
    Failed(String),
}

/// 单次工具调用记录
#[derive(Clone)]
pub struct ToolCallRecord {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result: Option<String>,
    pub status: ToolCallStatus,
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
    pub agent_mode: String,
}
