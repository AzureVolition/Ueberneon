// ── 书旁持久对话 ──
//
// 每本书一个全局对话：conversation 使用 status='sub_agent' 隐藏于普通列表，
// 走完整 Agent 管线（落库、usage、hook、审批），书内容只通过
// ReadBook / SearchBook / CiteBook 按需获取。

use crate::db::metadata::conversation::ConversationStatus;

/// 内置阅读助手 SubAgent 配置 ID。
pub const READ_HELPER_AGENT_ID: &str = "acfg-builtin-read-helper";

/// 书聊系统提示模板（构建 Agent 时按书填充）。
pub const BOOK_CHAT_SYSTEM_PROMPT: &str = r#"你是一位阅读辅助对话助手，正在陪用户阅读《${book_name}》（书 ID:${book_id}）。

规则：
1. 不要预读或复述整本书；需要书内内容时调用 ReadBook 按页读取，或用 SearchBook 搜索关键词。
2. 回答中只要引用了书中的具体内容，就必须调用 CiteBook 记录页码和原文片段，方便用户跳回原文。
3. 用户没有给出页码时，先搜索或向用户确认要讨论的位置，不要凭空编造书的内容。
4. 使用简体中文回答；公式用 LaTeX 表达。"#;

/// 生成带书信息的系统提示。
pub fn build_system_prompt(book_name: &str, book_id: &str) -> String {
    BOOK_CHAT_SYSTEM_PROMPT
        .replace("${book_name}", book_name)
        .replace("${book_id}", book_id)
}

/// 获取某本书的书聊 conversation；不存在则创建（含 book_chats 映射）。
pub fn ensure_conversation(book_id: &str) -> Result<String, String> {
    crate::db::with_db_result(|conn| -> Result<String, String> {
        if let Some(row) = crate::db::metadata::book_chat::get_by_book(conn, book_id)
            .map_err(|e| e.to_string())?
        {
            return Ok(row.conversation_id);
        }
        let book = crate::books::get(conn, book_id)
            .map_err(|e| format!("{e}"))?
            .ok_or_else(|| format!("book not found: {book_id}"))?;
        let cid = crate::db::metadata::conversation::create_with_status(
            conn,
            crate::db::DEFAULT_PROJECT_ID,
            &book.name,
            None,
            Some(READ_HELPER_AGENT_ID),
            ConversationStatus::SubAgent,
        )
        .map_err(|e| format!("create book conversation failed: {e}"))?;
        crate::db::metadata::book_chat::create(conn, book_id, &cid)
            .map_err(|e| format!("save book_chat mapping failed: {e}"))?;
        Ok(cid)
    })
}

/// 查询某本书已存在的书聊 conversation。
pub fn conversation_for(book_id: &str) -> Option<String> {
    crate::db::with_db(|conn| {
        crate::db::metadata::book_chat::get_by_book(conn, book_id)
            .ok()
            .flatten()
            .map(|r| r.conversation_id)
    })
}

/// 书聊 SubAgent 是否已配置 provider/model（决定阅读器是否显示「对话/解释」入口）。
pub fn configured() -> bool {
    crate::db::with_db(|conn| {
        crate::db::metadata::agent_config::get(conn, READ_HELPER_AGENT_ID)
            .ok()
            .flatten()
            .map(|row| crate::db::metadata::agent_config::subagent_effectively_configured(&row))
            .unwrap_or(false)
    })
}

/// 删除书时清理书聊：先删对话与消息，再删映射。
pub fn cleanup_for_book(book_id: &str) {
    let conv_id = crate::db::with_db(|conn| {
        crate::db::metadata::book_chat::get_by_book(conn, book_id)
            .ok()
            .flatten()
            .map(|r| r.conversation_id)
    });
    if let Some(cid) = conv_id {
        crate::db::try_with_db(|conn| {
            let _ = crate::db::metadata::message::delete_by_conversation(conn, &cid);
            let _ = crate::db::metadata::conversation::delete(conn, &cid);
            let _ = crate::db::metadata::book_chat::delete_by_conversation(conn, &cid);
        });
        crate::state_agent::manager::AgentManager::get().remove(&cid);
    }
    crate::db::try_with_db(|conn| {
        let _ = crate::db::metadata::book_chat::delete_by_book(conn, book_id);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_prompt_contains_book_context() {
        let p = build_system_prompt("高等数学", "book-1");
        assert!(p.contains("高等数学"), "{p}");
        assert!(p.contains("book-1"), "{p}");
        assert!(p.contains("SearchBook"), "{p}");
        assert!(p.contains("CiteBook"), "{p}");
    }
}
