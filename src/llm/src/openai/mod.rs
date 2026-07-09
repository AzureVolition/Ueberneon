//! OpenAI-compatible /chat/completions provider.
//!
//! 单文件实现 DeepSeek / MiMo / MiniMax / 通用 OpenAI 兼容端点。
//! DeepSeek: thinking.type="enabled" + reasoning_effort
//! MiniMax:  thinking.type="adaptive"|"disabled" (无 effort 标尺)

use std::time::Duration;
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::provider::{ChunkStream, Message, Provider, ProviderError, Request, Role, ToolSchema};
use crate::retry;
use crate::repair;

mod stream;
use stream::stream_with_reconnect;

// ── Provider 工厂 ──────────────────────────────────────────────────────────

/// OpenAI 兼容的 provider 实例。
pub struct OpenAiProvider {
    name: String,
    base_url: String,       // "https://api.deepseek.com"
    model: String,
    api_key: String,
    client: Client,
    deepseek: bool,         // DeepSeek: thinking.type="enabled"
    minimax: bool,          // MiniMax: thinking.type 只有 "adaptive" | "disabled"
    effort: String,         // reasoning_effort: low | medium | high
    vision: bool,
    vision_detail: String,  // low | high | ""
    idle_timeout: Duration,
}

impl OpenAiProvider {
    pub fn new(
        name: String,
        base_url: String,
        model: String,
        api_key: String,
        effort: Option<String>,
        vision: bool,
        vision_detail: Option<String>,
    ) -> Result<Self, ProviderError> {
        if base_url.is_empty() {
            return Err(ProviderError::Config("base_url is required".into()));
        }
        if model.is_empty() {
            return Err(ProviderError::Config("model is required".into()));
        }

        let client = Client::builder()
            .timeout(Duration::from_secs(300))
            .build()
            .map_err(|e| ProviderError::Config(format!("http client: {e}")))?;

        // 检测 endpoint 类型
        let deepseek = base_url.contains("api.deepseek.com");
        let minimax = !deepseek && base_url.contains("api.minimaxi.com");

        let effort = effort.unwrap_or_else(|| {
            if deepseek { "high".into() } else { String::new() }
        });

        Ok(Self {
            name,
            base_url,
            model,
            api_key,
            client,
            deepseek,
            minimax,
            effort,
            vision,
            vision_detail: vision_detail.unwrap_or_default(),
            idle_timeout: Duration::from_secs(120),
        })
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn stream(&self, req: &Request) -> Result<ChunkStream, ProviderError> {
        // 1. 构建请求体
        let body = self.build_request_body(req);

        // 2. 带重试发送 POST
        let resp = retry::send_with_retry(
            &self.client,
            &format!("{}/chat/completions", self.base_url),
            &self.api_key,
            &body,
        ).await?;

        // 3. 启动流解析 + 重连 goroutine
        let (tx, rx) = mpsc::channel(64);

        let client = self.client.clone();
        let base_url = self.base_url.clone();
        let api_key = self.api_key.clone();
        let body2 = body.clone();
        let idle_timeout = self.idle_timeout;

        tokio::spawn(async move {
            stream_with_reconnect(
                &client,
                &base_url,
                &api_key,
                &body2,
                resp,
                &tx,
                idle_timeout,
            ).await;
        });

        Ok(Box::pin(ReceiverStream::new(rx)))
    }
}

// ── 请求体构建 ──────────────────────────────────────────────────────────────

impl OpenAiProvider {
    fn build_request_body(&self, req: &Request) -> Value {
        let messages = self.build_messages(&req.messages);
        let tools = self.build_tools(&req.tools);

        let mut body = serde_json::json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
        });

        if let Some(tools) = tools {
            body["tools"] = tools;
        }

        // ── reasoning / thinking 配置 ──
        if self.deepseek {
            body["thinking"] = serde_json::json!({"type": "enabled"});
            if !self.effort.is_empty() {
                body["reasoning_effort"] = serde_json::json!(self.effort);
            }
        } else if self.minimax {
            let thinking_type = if self.effort == "off" || self.effort.is_empty() {
                "disabled"
            } else {
                "adaptive"
            };
            body["thinking"] = serde_json::json!({"type": thinking_type});
        } else if !self.effort.is_empty() {
            body["reasoning_effort"] = serde_json::json!(self.effort);
        }

        body
    }

    fn build_messages(&self, msgs: &[Message]) -> Vec<Value> {
        // 先修复 tool-call 配对（中断恢复场景）
        let msgs = repair::sanitize_tool_pairing(msgs);

        msgs.iter().map(|m| {
            let mut obj = serde_json::json!({
                "role": m.role,
            });

            // ── vision (多模态) ──
            if self.vision && m.role == Role::User && !m.images.is_empty() {
                let mut content: Vec<Value> = Vec::new();
                if let Some(ref text) = m.content {
                    if !text.is_empty() {
                        content.push(serde_json::json!({"type": "text", "text": text}));
                    }
                }
                for img in &m.images {
                    let mut image_obj = serde_json::json!({
                        "type": "image_url",
                        "image_url": {"url": img},
                    });
                    if !self.vision_detail.is_empty() {
                        image_obj["image_url"]["detail"] = serde_json::json!(self.vision_detail);
                    }
                    content.push(image_obj);
                }
                obj["content"] = serde_json::json!(content);
            } else {
                obj["content"] = serde_json::json!(m.content);
            }

            // ── DeepSeek: reasoning_content 回传 ──
            // tool_calls 回合必须带回 reasoning_content，否则 DeepSeek 400
            if self.deepseek
                && m.role == Role::Assistant
                && !m.tool_calls.is_empty()
                && m.reasoning_content.is_some()
            {
                obj["reasoning_content"] = serde_json::json!(m.reasoning_content);
            }

            // ── tool_calls ──
            if !m.tool_calls.is_empty() {
                let calls: Vec<Value> = m.tool_calls.iter().map(|tc| {
                    serde_json::json!({
                        "id": tc.id,
                        "type": "function",
                        "function": {
                            "name": tc.name,
                            "arguments": tc.arguments,
                        }
                    })
                }).collect();
                obj["tool_calls"] = serde_json::json!(calls);
            }

            // ── tool result ──
            if let Some(ref id) = m.tool_call_id {
                obj["tool_call_id"] = serde_json::json!(id);
            }
            if let Some(ref name) = m.name {
                obj["name"] = serde_json::json!(name);
            }

            obj
        }).collect()
    }

    fn build_tools(&self, tools: &[ToolSchema]) -> Option<Value> {
        if tools.is_empty() {
            return None;
        }
        let arr: Vec<Value> = tools.iter().map(|t| {
            serde_json::json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        }).collect();
        Some(serde_json::json!(arr))
    }
}

// ── SSE 事件类型 ─────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct SseChatResponse {
    choices: Vec<SseChoice>,
    #[serde(default)]
    usage: Option<SseUsage>,
}

#[derive(Debug, Deserialize)]
struct SseChoice {
    index: u32,
    delta: SseDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    reasoning_content: Option<String>,
    #[serde(default)]
    tool_calls: Vec<SseToolCall>,
}

#[derive(Debug, Deserialize)]
struct SseToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "function", default)]
    function: Option<SseFunction>,
}

#[derive(Debug, Default, Deserialize)]
struct SseFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SseUsage {
    prompt_tokens: u32,
    completion_tokens: u32,
    total_tokens: u32,
    #[serde(default)]
    prompt_tokens_details: Option<SsePromptTokenDetails>,
    #[serde(default)]
    completion_tokens_details: Option<SseCompletionTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct SsePromptTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u32>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct SseCompletionTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u32>,
}