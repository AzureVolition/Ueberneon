use std::collections::HashMap;
use std::io;
use std::time::Duration;

use futures::StreamExt;
use reqwest::Client;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::mpsc::Sender;
use tokio::time::timeout;

use crate::provider::{Chunk, ProviderError, ToolCall, Usage};
use crate::retry;

use super::{
    SseChatResponse
};

const MAX_STREAM_RECONNECTS: u32 = 3;

// ── 入口：带重连的流读取 ────────────────────────────────────────────────────

/// stream_with_reconnect 负责读取 SSE 流 + 连接断开后的重放。
///
///  
/// - 如果还没有发出任何 token → 可重放整个 HTTP 请求（最多 3 次）
/// - 如果已经发出了 token → 不能重放（会重复输出），标记 StreamInterrupted
pub async fn stream_with_reconnect(
    client: &Client,
    base_url: &str,
    api_key: &str,
    body: &Value,
    resp: reqwest::Response,
    tx: &Sender<Chunk>,
    idle_timeout: Duration,
) {
    let mut resp = resp;

    for attempt in 0..=MAX_STREAM_RECONNECTS {
        let (emitted, err) = read_stream(resp, tx, idle_timeout).await;

        if err.is_none() {
            return; // 正常结束
        }
        let err = err.unwrap();

        if !emitted && attempt < MAX_STREAM_RECONNECTS {
            // 还没发出任何 token，重放整个请求
            match retry::send_with_retry(client, base_url, api_key, body).await {
                Ok(new_resp) => {
                    resp = new_resp;
                    continue;
                }
                Err(e) => {
                    let _ = tx.send(Chunk::Error(e)).await;
                    return;
                }
            }
        } else {
            // 已发出 token 或重连次数耗尽
            let _ = tx.send(Chunk::Error(err)).await;
            return;
        }
    }
}

// ── SSE 流解析 ───────────────────────────────────────────────────────────────

/// 读取 SSE 流，解析 delta 事件，通过 tx 发送 Chunk。
/// 返回 (是否发出了 token, 错误)。
async fn read_stream(
    resp: reqwest::Response,
    tx: &Sender<Chunk>,
    idle_timeout: Duration,
) -> (bool, Option<ProviderError>) {
    // 将 resp 的字节流转为 AsyncRead
    let byte_stream = resp.bytes_stream().map(|r| {
        r.map_err(|e| io::Error::new(io::ErrorKind::Other, e))
    });
    let async_reader = tokio_util::io::StreamReader::new(byte_stream);
    let mut reader = BufReader::new(async_reader);
    let mut line = String::new();

    let mut emitted = false;
    let mut finish_reason = String::new();

    // 累积 tool_calls（增量拼接）
    let mut acc_tool_calls: HashMap<usize, ToolCallAcc> = HashMap::new();

    loop {
        // 带空闲超时读一行
        line.clear();
        let read_result = timeout(idle_timeout, reader.read_line(&mut line)).await;

        match read_result {
            Ok(Ok(0)) => break,                    // EOF
            Ok(Ok(_)) => { /* got a line */ }
            Ok(Err(e)) => {
                return (emitted, Some(ProviderError::StreamInterrupted(e)));
            }
            Err(_) => {
                // 超时
                return (emitted, Some(ProviderError::StreamInterrupted(
                    io::Error::new(io::ErrorKind::TimedOut, "stream idle timeout")
                )));
            }
        }

        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "data: [DONE]" {
            // 发送最后一个 usage（如果有 persistent cache 信息）
            break;
        }

        let Some(data) = line.strip_prefix("data: ") else {
            continue;
        };

        let chunk: SseChatResponse = match serde_json::from_str(data) {
            Ok(c) => c,
            Err(_) => continue,
        };

        // ── choices → delta 解析 ──
        if let Some(choice) = chunk.choices.first() {
            // 文本
            if let Some(ref content) = choice.delta.content {
                if !content.is_empty() {
                    emitted = true;
                    let _ = tx.send(Chunk::Text(content.clone())).await;
                }
            }

            // reasoning (thinking)
            if let Some(ref reasoning) = choice.delta.reasoning_content {
                if !reasoning.is_empty() {
                    emitted = true;
                    let _ = tx.send(Chunk::Reasoning {
                        text: reasoning.clone(),
                        signature: None,
                    }).await;
                }
            }

            // tool_calls (增量)
            for sse_tc in &choice.delta.tool_calls {
                emitted = true;
                let acc = acc_tool_calls.entry(sse_tc.index).or_default();

                // 首次出现：有 id → ToolCallStart
                if let Some(ref id) = sse_tc.id {
                    acc.id = id.clone();
                    let name = sse_tc.function.as_ref()
                        .and_then(|f| f.name.as_ref())
                        .cloned()
                        .unwrap_or_default();
                    let _ = tx.send(Chunk::ToolCallStart {
                        id: acc.id.clone(),
                        name,
                    }).await;
                }

                // 参数增量
                if let Some(ref func) = sse_tc.function {
                    if let Some(ref args) = func.arguments {
                        acc.args.push_str(args);
                        let _ = tx.send(Chunk::ToolCallDelta {
                            id: acc.id.clone(),
                            arguments: args.clone(),
                        }).await;
                    }
                }
            }

            // finish_reason
            if let Some(ref fr) = choice.finish_reason {
                finish_reason = fr.clone();
            }
        }

        // ── usage ──
        if let Some(ref u) = chunk.usage {
            let cache_hit = u.prompt_tokens_details.as_ref()
                .and_then(|d| d.cached_tokens)
                .unwrap_or(0);

            let cache_miss = if u.prompt_tokens >= cache_hit {
                u.prompt_tokens - cache_hit
            } else {
                0
            };

            let reasoning = u.completion_tokens_details.as_ref()
                .and_then(|d| d.reasoning_tokens)
                .unwrap_or(0);

            let _ = tx.send(Chunk::Usage(Usage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                cache_hit_tokens: cache_hit,
                cache_miss_tokens: cache_miss,
                reasoning_tokens: reasoning,
                finish_reason: finish_reason.clone(),
            })).await;
        }
    }

    // ── 发送最终的完整 tool_calls ──
    // 按 index 排序后发送 ToolCallComplete
    let mut indices: Vec<usize> = acc_tool_calls.keys().copied().collect();
    indices.sort();

    for idx in indices {
        let acc = &acc_tool_calls[&idx];
        if acc.id.is_empty() {
            continue;
        }
        let _ = tx.send(Chunk::ToolCallComplete(ToolCall {
            id: acc.id.clone(),
            name: acc.name.clone(),
            arguments: acc.args.clone(),
            diff: String::new(),
            added: 0,
            removed: 0,
        })).await;
    }

    (emitted, None)
}

// ── ToolCall 累积器 ─────────────────────────────────────────────────────────

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}