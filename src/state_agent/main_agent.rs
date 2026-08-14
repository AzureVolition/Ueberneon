// ── AgentCore 配置态方法（跨轮，不依赖运行态） ──
//
// 状态机重构后，跨轮能力（消息落库、usage 持久化、plan 收尾、输出提取）
// 统一收敛在 AgentCore 上，新旧执行路径复用，不写重复代码。

use super::AgentCore;
use super::InterruptState;
use llm::Role as LlmRole;

/// 书聊上下文压缩阈值：历史估算 token 达到上下文窗口该比例时触发。
pub const COMPRESS_THRESHOLD_RATIO: f64 = 0.7;
/// 压缩时始终保留的最近原始 user/assistant 轮次数。
pub const KEEP_RAW_EXCHANGES: usize = 6;
/// 摘要生成的最大 token 数。
pub const SUMMARY_MAX_TOKENS: u32 = 1200;

// ── Agent 配置态方法（跨轮，不依赖 Run）──────────────────────────────

impl AgentCore {
    /// 将 self.messages 转换为 DB 行（日后复用）
    pub fn to_message_rows(&self) -> Vec<crate::db::metadata::message::MessageRow> {
        self.messages
            .iter()
            .filter(|m| matches!(m.role, LlmRole::User | LlmRole::Assistant | LlmRole::Tool))
            .map(|m| crate::db::metadata::message::MessageRow::from_llm(m, &self.conversation_id))
            .collect()
    }

    /// 将单条 llm::Message 持久化到 messages 表（不删旧消息）。
    pub fn save_message(
        &self,
        conn: &rusqlite::Connection,
        msg: &llm::Message,
    ) -> rusqlite::Result<()> {
        use crate::db::metadata::message;
        let row = message::MessageRow::from_llm(msg, &self.conversation_id);
        message::create(conn, &self.conversation_id, &row)?;
        Ok(())
    }

    /// 单独更新 conversations.updated_at 为当前时间。
    pub fn touch_conversation(&self, conn: &rusqlite::Connection) -> rusqlite::Result<()> {
        conn.execute(
            "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![chrono::Local::now().to_rfc3339(), self.conversation_id],
        )?;
        Ok(())
    }

    /// 构造 Request 并请求 LLM 流（accept_message 首轮 / execute 续跑共用，避免重复代码）。
    /// 书聊在请求前先执行历史压缩。
    pub async fn request_stream(&mut self) -> Result<llm::provider::ChunkStream, InterruptState> {
        if self.book_chat {
            self.maybe_compress_history().await;
        }
        let req = llm::Request {
            messages: self.messages.clone(),
            tools: self.registry.schemas(),
            temperature: self.temperature,
            max_tokens: self.max_tokens.unwrap_or(65536),
        };
        self.provider
            .stream(&req)
            .await
            .map_err(|e| InterruptState::Error(format!("Stream error: {e}")))
    }

    /// 提取最后一次完成的 assistant 文本输出（子 Agent / 便捷路径用）。
    /// 跳过带 tool_calls 的中间轮（工具调用不是最终回答）。
    pub fn last_assistant_output(&self) -> String {
        self.messages
            .iter()
            .rev()
            .find(|m| m.role == llm::Role::Assistant && m.tool_calls.is_empty())
            .and_then(|m| m.content.clone())
            .unwrap_or_default()
    }

    /// 持久化 token 用量到 conversations 表（每次 LLM 交互后调用）。
    /// 落库逻辑收敛于此，流式/非流式路径复用，避免重复代码。
    pub fn persist_usage(&self, usage: &crate::model::TokenUsageRecord) {
        match crate::db::get_db().lock() {
            Ok(guard) => {
                if let Err(e) = crate::db::metadata::conversation::accumulate_usage(
                    &guard,
                    &self.conversation_id,
                    usage,
                ) {
                    tracing::warn!(target: "dashboard", error = %e, "accumulate_usage failed");
                }
            }
            Err(e) => {
                tracing::warn!(target: "dashboard", error = %e, "db lock failed for accumulate_usage");
            }
        }
    }
}

// ── 书聊上下文压缩 ─────────────────────────────────────────────────────────

impl AgentCore {
    /// 粗略估算消息历史 token 数（字符数/2 + 条数，CJK 友好）。
    fn estimate_tokens(messages: &[llm::Message]) -> u32 {
        let mut chars = 0usize;
        for m in messages {
            if let Some(c) = &m.content {
                chars += c.chars().count();
            }
            if let Some(c) = &m.reasoning_content {
                chars += c.chars().count();
            }
            for tc in &m.tool_calls {
                chars += tc.name.chars().count();
                chars += tc.arguments.chars().count();
            }
            if let Some(c) = &m.tool_name {
                chars += c.chars().count();
            }
        }
        (chars / 2 + messages.len()).max(1) as u32
    }

    /// 返回需要保留的最早消息索引（保留最近 KEEP_RAW_EXCHANGES 组原文）。
    fn compression_keep_from(&self) -> Option<usize> {
        if !self.book_chat {
            return None;
        }
        let estimated = Self::estimate_tokens(&self.messages);
        let limit = (self.context_window as f64 * COMPRESS_THRESHOLD_RATIO) as u32;
        if estimated < limit {
            return None;
        }
        let user_idx: Vec<usize> = self
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == LlmRole::User)
            .map(|(i, _)| i)
            .collect();
        if user_idx.len() <= KEEP_RAW_EXCHANGES {
            return None;
        }
        Some(user_idx[user_idx.len() - KEEP_RAW_EXCHANGES])
    }

    /// 用同一 provider 把最早历史压缩成中文摘要；失败只记日志，不中断请求。
    async fn maybe_compress_history(&mut self) {
        let Some(keep_from) = self.compression_keep_from() else {
            return;
        };
        let prefix = self
            .messages
            .iter()
            .take_while(|m| m.role == LlmRole::System)
            .count();
        if keep_from <= prefix {
            return;
        }

        let compressable: Vec<llm::Message> = self.messages[prefix..keep_from].to_vec();
        let summary = match self.summarize_history(&compressable).await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(target: "agent", conversation_id = %self.conversation_id, error = %e, "book chat compression failed");
                return;
            }
        };
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            return;
        }

        let cutoff = compressable
            .iter()
            .filter_map(|m| m.timestamp)
            .max()
            .unwrap_or_else(chrono::Utc::now);
        let summary_msg = llm::Message {
            role: LlmRole::System,
            content: Some(format!("【历史对话摘要】\n{summary}")),
            timestamp: Some(chrono::Utc::now()),
            ..Default::default()
        };

        self.messages.drain(prefix..keep_from);
        self.messages.insert(prefix, summary_msg.clone());

        if let Ok(guard) = crate::db::get_db().lock() {
            let _ = crate::db::metadata::message::mark_compressed_before(
                &guard,
                &self.conversation_id,
                &cutoff.to_rfc3339(),
            );
            let _ = self.save_message(&guard, &summary_msg);
            let _ = self.touch_conversation(&guard);
        }
        tracing::info!(
            target: "agent",
            conversation_id = %self.conversation_id,
            compressed = compressable.len(),
            "book chat history compressed"
        );
    }

    /// 调用同 provider 的流式接口生成摘要（无工具、低温度）。
    async fn summarize_history(&self, history: &[llm::Message]) -> Result<String, String> {
        use futures::StreamExt;

        let rendered = history
            .iter()
            .map(|m| {
                let content = m.content.as_deref().unwrap_or("");
                match m.role {
                    LlmRole::User => format!("用户: {content}"),
                    LlmRole::Assistant => format!("助手: {content}"),
                    LlmRole::Tool => format!("工具({}): {content}", m.tool_name.as_deref().unwrap_or("?")),
                    LlmRole::System => content.to_string(),
                }
            })
            .collect::<Vec<_>>()
            .join("\n");

        let req = llm::Request {
            messages: vec![
                llm::Message {
                    role: LlmRole::System,
                    content: Some(
                        "你是对话压缩器。把用户提供的书聊历史压缩为简体中文摘要，\
                         必须保留：已解答的问题与关键结论、未解决的问题、引用过的页码与原文要点。\
                         不要复述工具 JSON，不要输出思考过程。"
                            .to_string(),
                    ),
                    ..Default::default()
                },
                llm::Message {
                    role: LlmRole::User,
                    content: Some(format!("以下是待压缩的书聊历史：\n\n{rendered}\n\n请输出压缩摘要。")),
                    ..Default::default()
                },
            ],
            tools: Vec::new(),
            temperature: 0.2,
            max_tokens: SUMMARY_MAX_TOKENS,
        };

        let mut stream = self
            .provider
            .stream(&req)
            .await
            .map_err(|e| format!("summarize request failed: {e}"))?;
        let mut out = String::new();
        while let Some(chunk) = stream.next().await {
            if let Ok(llm::Chunk::Text(t)) = chunk {
                out.push_str(&t);
            }
        }
        Ok(out)
    }

    #[cfg(test)]
    fn test_messages(count: usize) -> Vec<llm::Message> {
        (0..count)
            .map(|i| llm::Message {
                role: if i % 2 == 0 { LlmRole::User } else { LlmRole::Assistant },
                content: Some(format!("第 {i} 轮问题与回答内容").repeat(20)),
                timestamp: Some(chrono::Utc::now()),
                ..Default::default()
            })
            .collect()
    }
}

// ── Arc 辅助操作 ─────────────────────────────────────────────────────────────
pub fn defautlt_main_agent_prompt() -> String {
    "You are a helpful assistant. Current workspace: ${workspace_path}.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compression_keeps_recent_exchanges() {
        let core = AgentCore {
            provider: Box::new(
                llm::OpenAiProvider::new(
                    "test".into(),
                    "http://localhost:1".into(),
                    "test".into(),
                    "".into(),
                    None,
                    false,
                    None,
                )
                .unwrap(),
            ),
            registry: std::sync::Arc::new(crate::tools::Registry::new()),
            project_path: std::path::PathBuf::from("/tmp"),
            project_id: None,
            conversation_id: "c".into(),
            messages: AgentCore::test_messages(20),
            temperature: 0.7,
            max_tokens: None,
            context_window: 1000,
            agent_type: "SubAgent".into(),
            last_usage: None,
            book_chat: true,
        };
        let keep_from = core.compression_keep_from().expect("should compress");
        let user_indices: Vec<usize> = core
            .messages
            .iter()
            .enumerate()
            .filter(|(_, m)| m.role == LlmRole::User)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(user_indices[user_indices.len() - KEEP_RAW_EXCHANGES], keep_from);
    }

    #[test]
    fn estimate_tokens_is_positive() {
        let msgs = vec![llm::Message {
            role: LlmRole::User,
            content: Some("你好".into()),
            ..Default::default()
        }];
        assert!(AgentCore::estimate_tokens(&msgs) >= 1);
    }
}
