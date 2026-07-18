// ── Message CRUD（对齐 llm::Message 格式）──
// 支持 active 字段：active / compressed，查询只返回 active 的消息。

use chrono::{DateTime, Local};
use rusqlite::{params, Connection, Result};

use llm::{Message as LlmMessage, Role as LlmRole, ToolCall};

/// 消息状态
#[derive(Debug, Clone, PartialEq)]
pub enum MessageStatus {
    Active,
    Compressed,
}

impl MessageStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageStatus::Active => "active",
            MessageStatus::Compressed => "compressed",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "compressed" => MessageStatus::Compressed,
            _ => MessageStatus::Active,
        }
    }
}

/// 数据库行 —— 对齐 llm::Message 格式
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub id: i64,
    pub conversation_id: String,
    pub role: String,
    pub content: Option<String>,
    pub timestamp: DateTime<Local>,
    pub reasoning_content: Option<String>,
    pub reasoning_signature: Option<String>,
    pub tool_calls: Option<String>,  // JSON array of ToolCall
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub images: Option<String>,      // JSON array of base64 strings
    pub active: MessageStatus,
}

// ── 与 llm::Message 互转 ──────────────────────────────────────────────────

impl MessageRow {
    /// 从 llm::Message 创建行（需外部提供 conversation_id）
    pub fn from_llm(msg: &LlmMessage, conversation_id: &str) -> Self {
        let role_str = match msg.role {
            LlmRole::System => "system",
            LlmRole::User => "user",
            LlmRole::Assistant => "assistant",
            LlmRole::Tool => "tool",
        }
        .to_string();

        let tool_calls = if msg.tool_calls.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&msg.tool_calls).unwrap_or_else(|_| "[]".into()))
        };

        let images = if msg.images.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&msg.images).unwrap_or_else(|_| "[]".into()))
        };

        MessageRow {
            id: 0,
            conversation_id: conversation_id.to_string(),
            role: role_str,
            content: msg.content.clone(),
            timestamp: msg.timestamp
                .map(|t| t.with_timezone(&Local))
                .unwrap_or_else(Local::now),
            reasoning_content: msg.reasoning_content.clone(),
            reasoning_signature: msg.reasoning_signature.clone(),
            tool_calls,
            tool_call_id: msg.tool_call_id.clone(),
            tool_name: msg.tool_name.clone(),
            images,
            active: MessageStatus::Active,
        }
    }

    /// 转换为 llm::Message
    pub fn to_llm(&self) -> LlmMessage {
        let role = match self.role.as_str() {
            "system" => LlmRole::System,
            "user" => LlmRole::User,
            "assistant" => LlmRole::Assistant,
            "tool" => LlmRole::Tool,
            _ => LlmRole::User,
        };

        let tool_calls: Vec<ToolCall> = self
            .tool_calls
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        let images: Vec<String> = self
            .images
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();

        LlmMessage {
            role,
            content: self.content.clone(),
            reasoning_content: self.reasoning_content.clone(),
            reasoning_signature: self.reasoning_signature.clone(),
            tool_calls,
            tool_call_id: self.tool_call_id.clone(),
            tool_name: self.tool_name.clone(),
            images,
            timestamp: None,
        }
    }
}

// ── CRUD ──────────────────────────────────────────────────────────────────

/// 创建消息，返回自增 id
pub fn create(conn: &Connection, conversation_id: &str, row: &MessageRow) -> Result<i64> {
    let ts = row.timestamp.to_rfc3339();
    conn.execute(
        "INSERT INTO messages
            (conversation_id, role, content, timestamp,
             reasoning_content, reasoning_signature,
             tool_calls, tool_call_id, tool_name, images, active)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            conversation_id,
            row.role,
            row.content,
            ts,
            row.reasoning_content,
            row.reasoning_signature,
            row.tool_calls,
            row.tool_call_id,
            row.tool_name,
            row.images,
            row.active.as_str(),
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 便捷函数：从 llm::Message 创建并持久化
pub fn create_from_llm(
    conn: &Connection,
    conversation_id: &str,
    msg: &LlmMessage,
) -> Result<i64> {
    let row = MessageRow::from_llm(msg, conversation_id);
    create(conn, conversation_id, &row)
}

/// 按 id 查询消息（不限 active）
pub fn get(conn: &Connection, id: i64) -> Result<Option<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, role, content, timestamp,
                reasoning_content, reasoning_signature,
                tool_calls, tool_call_id, tool_name, images, active
         FROM messages WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], row_mapper)?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 列出某对话下所有 active 消息，按 timestamp 升序
pub fn list_by_conversation(conn: &Connection, conversation_id: &str) -> Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, role, content, timestamp,
                reasoning_content, reasoning_signature,
                tool_calls, tool_call_id, tool_name, images, active
         FROM messages
         WHERE conversation_id = ?1 AND active = 'active'
         ORDER BY timestamp",
    )?;
    let rows = stmt.query_map(params![conversation_id], row_mapper)?;
    rows.collect()
}

/// 列出某对话下所有消息（不限 active），按 timestamp 升序
pub fn list_all_by_conversation(conn: &Connection, conversation_id: &str) -> Result<Vec<MessageRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, conversation_id, role, content, timestamp,
                reasoning_content, reasoning_signature,
                tool_calls, tool_call_id, tool_name, images, active
         FROM messages WHERE conversation_id = ?1
         ORDER BY timestamp",
    )?;
    let rows = stmt.query_map(params![conversation_id], row_mapper)?;
    rows.collect()
}

/// 将某对话下所有 active 消息转换为 llm::Message 列表
pub fn list_as_llm_messages(
    conn: &Connection,
    conversation_id: &str,
) -> Result<Vec<LlmMessage>> {
    let rows = list_by_conversation(conn, conversation_id)?;
    Ok(rows.iter().map(|r| r.to_llm()).collect())
}

/// 删除消息
pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM messages WHERE id=?1", params![id])?;
    Ok(())
}

/// 删除某对话下所有消息
pub fn delete_by_conversation(conn: &Connection, conversation_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM messages WHERE conversation_id=?1",
        params![conversation_id],
    )?;
    Ok(())
}

// ── 行映射器 ───────────────────────────────────────────────────────────────

fn row_mapper(row: &rusqlite::Row<'_>) -> rusqlite::Result<MessageRow> {
    let ts_str: String = row.get(4)?;
    Ok(MessageRow {
        id: row.get(0)?,
        conversation_id: row.get(1)?,
        role: row.get(2)?,
        content: row.get(3)?,
        timestamp: DateTime::parse_from_rfc3339(&ts_str)
            .map(|dt| dt.with_timezone(&Local))
            .unwrap_or_else(|_| Local::now()),
        reasoning_content: row.get(5)?,
        reasoning_signature: row.get(6)?,
        tool_calls: row.get(7)?,
        tool_call_id: row.get(8)?,
        tool_name: row.get(9)?,
        images: row.get(10)?,
        active: MessageStatus::from_str(&row.get::<_, String>(11)?),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::metadata::{conversation, project};

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL,
                created_at TEXT NOT NULL, indicator_color TEXT DEFAULT '',
                last_activity_at TEXT
            );
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
                title TEXT DEFAULT '', updated_at TEXT NOT NULL,
                agent_config_id TEXT
            );
            CREATE TABLE messages (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id     TEXT NOT NULL REFERENCES conversations(id),
                role                TEXT NOT NULL,
                content             TEXT,
                timestamp           TEXT NOT NULL,
                reasoning_content   TEXT,
                reasoning_signature TEXT,
                tool_calls          TEXT,
                tool_call_id        TEXT,
                tool_name           TEXT,
                images              TEXT,
                active              TEXT NOT NULL DEFAULT 'active'
            );",
        )
        .unwrap();
        conn
    }

    fn create_test_msg(conn: &Connection, cid: &str, role: LlmRole, content: &str) -> i64 {
        create_from_llm(conn, cid, &LlmMessage {
            role,
            content: Some(content.into()),
            ..Default::default()
        }).unwrap()
    }

    #[test]
    fn test_create_user_message() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let cid = conversation::create(&conn, &pid, "c", None).unwrap();

        let llm_msg = LlmMessage {
            role: LlmRole::User,
            content: Some("hello".into()),
            ..Default::default()
        };
        let id = create_from_llm(&conn, &cid, &llm_msg).unwrap();
        assert!(id > 0);

        let row = get(&conn, id).unwrap().expect("should exist");
        assert_eq!(row.role, "user");
        assert_eq!(row.content.as_deref(), Some("hello"));
        assert_eq!(row.active, MessageStatus::Active);
    }

    #[test]
    fn test_list_filters_active() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let cid = conversation::create(&conn, &pid, "c", None).unwrap();

        let id1 = create_test_msg(&conn, &cid, LlmRole::User, "active msg");
        // 手动插入一条 compressed 消息
        conn.execute(
            "INSERT INTO messages (conversation_id, role, content, timestamp, active)
             VALUES (?1, 'user', 'compressed msg', ?2, 'compressed')",
            params![cid, chrono::Local::now().to_rfc3339()],
        ).unwrap();

        let rows = list_by_conversation(&conn, &cid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].content.as_deref(), Some("active msg"));

        let all = list_all_by_conversation(&conn, &cid).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_create_assistant_with_tool_calls() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let cid = conversation::create(&conn, &pid, "c", None).unwrap();

        let llm_msg = LlmMessage {
            role: LlmRole::Assistant,
            content: None,
            reasoning_content: Some("thinking...".into()),
            tool_calls: vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"/tmp/x"}"#.into(),
                diff: String::new(),
                added: 0,
                removed: 0,
            }],
            ..Default::default()
        };
        let id = create_from_llm(&conn, &cid, &llm_msg).unwrap();

        let row = get(&conn, id).unwrap().expect("should exist");
        assert_eq!(row.role, "assistant");
        assert!(row.content.is_none());
        assert_eq!(row.reasoning_content.as_deref(), Some("thinking..."));
        assert!(row.tool_calls.is_some());

        let roundtrip = row.to_llm();
        assert_eq!(roundtrip.role, LlmRole::Assistant);
        assert_eq!(roundtrip.content, None);
        assert_eq!(roundtrip.tool_calls.len(), 1);
        assert_eq!(roundtrip.tool_calls[0].name, "read_file");
    }

    #[test]
    fn test_create_tool_result() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let cid = conversation::create(&conn, &pid, "c", None).unwrap();

        let llm_msg = LlmMessage {
            role: LlmRole::Tool,
            content: Some("file contents".into()),
            tool_call_id: Some("call_1".into()),
            tool_name: Some("read_file".into()),
            ..Default::default()
        };
        let id = create_from_llm(&conn, &cid, &llm_msg).unwrap();
        let row = get(&conn, id).unwrap().expect("should exist");
        assert_eq!(row.role, "tool");
        assert_eq!(row.tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(row.tool_name.as_deref(), Some("read_file"));
    }

    #[test]
    fn test_list_as_llm_messages() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let cid = conversation::create(&conn, &pid, "c", None).unwrap();

        create_from_llm(
            &conn,
            &cid,
            &LlmMessage {
                role: LlmRole::User,
                content: Some("hi".into()),
                ..Default::default()
            },
        )
        .unwrap();
        create_from_llm(
            &conn,
            &cid,
            &LlmMessage {
                role: LlmRole::Assistant,
                content: Some("hello!".into()),
                ..Default::default()
            },
        )
        .unwrap();

        let msgs = list_as_llm_messages(&conn, &cid).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].content.as_deref(), Some("hi"));
        assert_eq!(msgs[1].content.as_deref(), Some("hello!"));
    }

    #[test]
    fn test_delete() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let cid = conversation::create(&conn, &pid, "c", None).unwrap();
        let llm_msg = LlmMessage {
            role: LlmRole::User,
            content: Some("x".into()),
            ..Default::default()
        };
        let id = create_from_llm(&conn, &cid, &llm_msg).unwrap();
        delete(&conn, id).unwrap();
        assert!(get(&conn, id).unwrap().is_none());
    }

    #[test]
    fn test_delete_by_conversation() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let cid = conversation::create(&conn, &pid, "c", None).unwrap();

        for i in 0..3 {
            create_from_llm(
                &conn,
                &cid,
                &LlmMessage {
                    role: LlmRole::User,
                    content: Some(format!("msg {i}")),
                    ..Default::default()
                },
            )
            .unwrap();
        }
        delete_by_conversation(&conn, &cid).unwrap();
        let rows = list_by_conversation(&conn, &cid).unwrap();
        assert_eq!(rows.len(), 0);
    }
}
