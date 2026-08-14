// ── 书旁持久对话 ──
//
// 每本书一个全局对话：conversation 使用 status='sub_agent' 隐藏于普通列表，
// 走完整 Agent 管线（落库、usage、hook、审批），书内容只通过
// ReadBook / SearchBook / CiteBook 按需获取。

use crate::db::metadata::conversation::ConversationStatus;

/// 内置阅读助手 SubAgent 配置 ID。
pub const READ_HELPER_AGENT_ID: &str = "acfg-builtin-read-helper";

/// 阅读环境 system 消息标记，用于识别并比较“上次已发送”的环境变量。
pub const READING_ENV_MARKER: &str = "【阅读环境】";

/// 书聊系统提示模板（构建 Agent 时按书填充）。
pub const BOOK_CHAT_SYSTEM_PROMPT: &str = r#"你是一位阅读辅助对话助手，正在陪用户阅读《${book_name}》（书 ID:${book_id}）。
当前学习计划：${plan_name}（计划 ID:${plan_id}）。
当前学习计划引用的书籍：
${plan_books}

规则：
1. 不要预读或复述整本书；需要书内内容时调用 ReadBook 按页读取，或用 SearchBook 搜索关键词。
2. 回答中只要引用了书中的具体内容，就必须调用 CiteBook 记录页码和原文片段，方便用户跳回原文。
3. 用户没有给出页码时，先搜索或向用户确认要讨论的位置，不要凭空编造书的内容。
4. 使用简体中文回答；公式用 LaTeX 表达。"#;

/// 生成带书信息的系统提示。
pub fn build_system_prompt(
    book_name: &str,
    book_id: &str,
    plan_name: &str,
    plan_id: &str,
    plan_books: &[crate::books::BookRow],
) -> String {
    let books_section = if plan_books.is_empty() {
        "当前学习计划暂未引入书籍。".to_string()
    } else {
        plan_books
            .iter()
            .map(|b| format!("- 《{}》（书 ID:{}）", b.name, b.id))
            .collect::<Vec<_>>()
            .join("\n")
    };
    BOOK_CHAT_SYSTEM_PROMPT
        .replace("${book_name}", book_name)
        .replace("${book_id}", book_id)
        .replace("${plan_name}", plan_name)
        .replace("${plan_id}", plan_id)
        .replace("${plan_books}", &books_section)
}

/// 阅读环境历史信息快照（结构化，不解析字符串）。
#[derive(Clone, Debug, PartialEq)]
pub struct ReadingEnvSnapshot {
    pub book_id: String,
    pub book_name: String,
    pub plan_id: String,
    pub plan_name: String,
    pub plan_books: Vec<(String, String)>,
}

impl ReadingEnvSnapshot {
    /// 从 DB 组装快照：当前书、学习计划、计划引用的书籍。
    pub fn from_ids(book_id: &str, project_id: Option<&str>) -> Self {
        let book = crate::db::with_db(|conn| {
            crate::books::get(conn, book_id).ok().flatten()
        });
        let (plan_name, plan_books) = match project_id {
            Some(pid) => {
                let name = crate::db::with_db(|conn| {
                    crate::db::metadata::project::get(conn, pid)
                        .ok()
                        .flatten()
                        .map(|r| r.name)
                        .unwrap_or_default()
                });
                let books = crate::db::with_db(|conn| {
                    crate::books::list_by_project(conn, pid).unwrap_or_default()
                });
                (name, books)
            }
            None => (String::new(), Vec::new()),
        };
        Self {
            book_id: book_id.to_string(),
            book_name: book.map(|b| b.name).unwrap_or_default(),
            plan_id: project_id.unwrap_or_default().to_string(),
            plan_name,
            plan_books: plan_books
                .into_iter()
                .map(|b| (b.id, b.name))
                .collect(),
        }
    }

    /// 生成 system 前置消息（由快照直接生成，不解析历史字符串）。
    pub fn system_message(&self) -> String {
        let mut out = format!(
            "{READING_ENV_MARKER}\n当前书籍：《{}》（书 ID:{}）\n当前学习计划：{}（计划 ID:{}）\n学习计划引用的书籍：\n",
            self.book_name, self.book_id, self.plan_name, self.plan_id
        );
        if self.plan_books.is_empty() {
            out.push_str("当前学习计划暂未引入书籍。");
        } else {
            for (id, name) in &self.plan_books {
                out.push_str(&format!("- 《{name}》（书 ID:{id}）\n"));
            }
        }
        out
    }
}

/// 阅读环境变量合集（内存态，不落库）。
pub struct ReadingEnvState {
    snapshot: ReadingEnvSnapshot,
    /// 上一次已发送的环境消息（历史信息快照）。
    /// 对比结束后即被当前消息替换，不保留历史记录。
    previous_message: Option<String>,
}

impl ReadingEnvState {
    pub fn new(snapshot: ReadingEnvSnapshot) -> Self {
        Self {
            snapshot,
            previous_message: None,
        }
    }

    /// 更新快照（内容变化后，下一次 take_preamble 会重新发送）。
    pub fn update(&mut self, snapshot: ReadingEnvSnapshot) {
        self.snapshot = snapshot;
    }

    /// 取本轮应发送的 system 前置消息：
    /// - 未发送过 / 环境内容变了 → Some(消息)，并舍弃旧快照、记录当前消息。
    /// - 内容未变 → None。
    pub fn take_preamble(&mut self) -> Option<String> {
        let msg = self.snapshot.system_message();
        if self.previous_message.as_deref() == Some(&msg) {
            return None;
        }
        self.previous_message = Some(msg.clone());
        Some(msg)
    }
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

/// 删除一个对话（含消息软删除、对话软删除、Agent 缓存清理）。
/// 阅读对话与主对话共用。
pub fn delete_conversation(conversation_id: &str) {
    crate::db::try_with_db(|conn| {
        let _ = crate::db::metadata::message::delete_by_conversation(conn, conversation_id);
        let _ = crate::db::metadata::conversation::delete(conn, conversation_id);
    });
    crate::state_agent::manager::AgentManager::get().remove(conversation_id);
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
        let books = vec![crate::books::BookRow {
            id: "book-a".into(),
            name: "代数".into(),
            path: "/tmp/a".into(),
            created_at: "t".into(),
        }];
        let p = build_system_prompt("高等数学", "book-1", "代数学习计划", "plan-9", &books);
        assert!(p.contains("高等数学"), "{p}");
        assert!(p.contains("book-1"), "{p}");
        assert!(p.contains("代数学习计划"), "{p}");
        assert!(p.contains("plan-9"), "{p}");
        assert!(p.contains("《代数》"), "{p}");
        assert!(p.contains("book-a"), "{p}");
        assert!(p.contains("SearchBook"), "{p}");
        assert!(p.contains("CiteBook"), "{p}");
    }

    #[test]
    fn system_prompt_mentions_empty_plan_books() {
        let p = build_system_prompt("书", "b1", "计划", "p1", &[]);
        assert!(p.contains("暂未引入书籍"), "{p}");
    }

    #[test]
    fn reading_env_system_message_contains_snapshot_context() {
        let snapshot = ReadingEnvSnapshot {
            book_id: "book-42".into(),
            book_name: "高等数学".into(),
            plan_id: "plan-9".into(),
            plan_name: "代数学习计划".into(),
            plan_books: vec![("book-a".into(), "代数".into())],
        };
        let msg = snapshot.system_message();
        assert!(msg.starts_with(READING_ENV_MARKER), "{msg}");
        assert!(msg.contains("book-42"), "{msg}");
        assert!(msg.contains("高等数学"), "{msg}");
        assert!(msg.contains("代数学习计划"), "{msg}");
        assert!(msg.contains("《代数》"), "{msg}");
    }

    #[test]
    fn reading_env_take_preamble_only_on_first_or_change() {
        let s1 = ReadingEnvSnapshot {
            book_id: "book-1".into(),
            book_name: "书一".into(),
            plan_id: String::new(),
            plan_name: String::new(),
            plan_books: Vec::new(),
        };
        let s2 = ReadingEnvSnapshot {
            book_id: "book-2".into(),
            book_name: "书二".into(),
            plan_id: String::new(),
            plan_name: String::new(),
            plan_books: Vec::new(),
        };
        let mut env = ReadingEnvState::new(s1);
        assert!(env.take_preamble().is_some(), "首次应发送");
        assert!(env.take_preamble().is_none(), "同一内容未变化不发送");
        env.update(s2);
        assert!(env.take_preamble().is_some(), "环境变化应重发");
        assert!(env.take_preamble().is_none(), "重发后回到未变化");
    }
}
