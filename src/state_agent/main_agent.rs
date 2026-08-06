// ── AgentCore 配置态方法（跨轮，不依赖运行态） ──
//
// 状态机重构后，跨轮能力（消息落库、usage 持久化、plan 收尾、输出提取）
// 统一收敛在 AgentCore 上，新旧执行路径复用，不写重复代码。

use super::AgentCore;
use super::InterruptState;
use llm::Role as LlmRole;

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
    pub async fn request_stream(&self) -> Result<llm::provider::ChunkStream, InterruptState> {
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

// ── Arc 辅助操作 ─────────────────────────────────────────────────────────────
pub fn defautlt_main_agent_prompt() -> String {
    "You are a helpful assistant. Current workspace: ${workspace_path}.".to_string()
}
