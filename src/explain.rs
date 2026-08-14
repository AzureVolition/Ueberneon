// ── 阅读器选区解释 ──
//
// 与 translate 同构:解释子代理是一个 agent_config 行(acfg-builtin-explain),
// provider/model 在设置 → Sub Agents 中配置;阅读器操作栏「解释」用完整
// 子 Agent 执行,子 Agent 可通过 ReadBook 工具查阅书中其他位置。

use crate::db::metadata::agent_config::AgentConfigRow;

/// 内置解释子代理的固定 ID。
pub const EXPLAIN_AGENT_ID: &str = "acfg-builtin-explain";

/// 内置解释子代理的系统提示。
pub const EXPLAIN_SYSTEM_PROMPT: &str = "你是一位阅读辅助解释助手。用户会给出书中一段文本(可能含 LaTeX 公式)。请用简体中文解释这段文本的含义、背景与关键概念;如果不够确定,可以调用 ReadBook 工具查阅书中其他位置的上下文或定义,引用书中内容时标注页码。只输出解释本身,不要复述用户原文,不要输出思考过程。";

/// 返回内置解释子代理配置行;未配置 provider/模型时返回 None。
pub fn explain_agent() -> Option<AgentConfigRow> {
    crate::db::with_db(|conn| {
        let agent = crate::db::metadata::agent_config::get(conn, EXPLAIN_AGENT_ID)
            .ok()
            .flatten()?;
        if agent.agent_type != "SubAgent"
            || !crate::db::metadata::agent_config::subagent_effectively_configured(&agent)
        {
            return None;
        }
        Some(agent)
    })
}

/// 构造解释请求的用户消息:包含书名、书 ID、页码与选中文本。
/// 附上书 ID 让子 Agent 调 ReadBook 时能精确命中,不受长书名截断影响。
pub fn build_prompt(source: &str, book_name: &str, book_id: &str, page: u32) -> String {
    format!(
        "请解释下面这段选自《{book_name}》(书 ID:{book_id})第 {page} 页的文本(公式已用 LaTeX 表示):\n\n{source}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_prompt_includes_source_book_and_page() {
        let prompt = build_prompt("E = mc²\n\n定义……", "相对论", "book-123", 42);
        assert!(prompt.contains("相对论"), "{prompt}");
        assert!(prompt.contains("book-123"), "{prompt}");
        assert!(prompt.contains("第 42 页"), "{prompt}");
        assert!(prompt.contains("E = mc²"), "{prompt}");
    }
}
