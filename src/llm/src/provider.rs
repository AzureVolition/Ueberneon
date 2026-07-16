use std::fmt;

use futures::stream::Stream;
use serde::{Serialize, Deserialize};
use std::pin::Pin;

pub type ChunkStream = Pin<Box<dyn Stream<Item = Result<Chunk, ProviderError>> + Send>>;

#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    async fn stream(&self, req: &Request) -> Result<ChunkStream, ProviderError>;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct Message {
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_signature: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,        // tool result 消息的工具名
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub images: Vec<String>,         // data:image/...;base64,...
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,   // raw JSON string
    pub diff: String,
    pub added: u32,
    pub removed: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,  // JSON Schema
}

#[derive(Clone, Debug)]
pub struct Request {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSchema>,
    pub temperature: f64,
    pub max_tokens: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Chunk {
    Text(String),
    Reasoning { text: String, signature: Option<String> },
    ToolCallStart { id: String, name: String },
    ToolCallDelta { id: String, arguments: String },
    ToolCallComplete(ToolCall),
    Usage(Usage),
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
    pub cache_hit_tokens: u32,
    pub cache_miss_tokens: u32,
    pub reasoning_tokens: u32,
    pub finish_reason: String,
}

// ── ProviderError ──

#[derive(Debug)]
pub enum ProviderError {
    Config(String),
    HttpStatus(u16),
    Network(reqwest::Error),
    StreamInterrupted(std::io::Error),
    Auth {
        provider: String,
        key_env: Option<String>,
        status: u16,
    },
    Json(serde_json::Error),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Config(msg) => write!(f, "config: {msg}"),
            Self::HttpStatus(s) => write!(f, "HTTP {s}"),
            Self::Network(e) => write!(f, "network: {e}"),
            Self::StreamInterrupted(e) => write!(f, "stream interrupted: {e}"),
            Self::Auth { provider, status, .. } => {
                write!(f, "auth failed for {provider} (HTTP {status})")
            }
            Self::Json(e) => write!(f, "json: {e}"),
        }
    }
}

impl std::error::Error for ProviderError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Network(e) => Some(e),
            Self::StreamInterrupted(e) => Some(e),
            Self::Json(e) => Some(e),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for ProviderError {
    fn from(e: serde_json::Error) -> Self { Self::Json(e) }
}

impl fmt::Display for Role {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Role::System => write!(f, "system"),
            Role::User => write!(f, "user"),
            Role::Assistant => write!(f, "assistant"),
            Role::Tool => write!(f, "tool"),
        }
    }
}


