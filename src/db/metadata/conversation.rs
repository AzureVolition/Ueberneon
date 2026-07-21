// ── Conversation CRUD ──

use chrono::{DateTime, Local};
use rusqlite::{params, Connection, Result};

/// 对话状态
#[derive(Debug, Clone, PartialEq, Default)]
pub enum ConversationStatus {
    #[default]
    Active,
    Deleted,
    SubAgent,
}

impl ConversationStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ConversationStatus::Active => "active",
            ConversationStatus::Deleted => "deleted",
            ConversationStatus::SubAgent => "sub_agent",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "deleted" => ConversationStatus::Deleted,
            "sub_agent" => ConversationStatus::SubAgent,
            _ => ConversationStatus::Active,
        }
    }
}

/// 数据库行 —— 不含嵌套的 messages 列表
#[derive(Debug, Clone)]
pub struct ConversationRow {
    pub id: String,
    pub project_id: String,
    pub parent_conversation_id: Option<String>,
    pub title: String,
    pub created_at: DateTime<Local>,
    pub updated_at: DateTime<Local>,
    pub message_count: i64,
    pub agent_config_id: Option<String>,
    pub status: ConversationStatus,
}

use std::sync::atomic::AtomicU16;

static ID_COUNTER: AtomicU16 = AtomicU16::new(0);

/// 生成形如 `conv-1748612345678-xxxx` 的 id
pub fn generate_conversation_id() -> String {
    let millis = chrono::Local::now().timestamp_millis();
    let seq = ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 9999;
    format!("conv-{millis}-{seq}")
}

/// 创建对话，返回新 id
pub fn create(
    conn: &Connection,
    project_id: &str,
    title: &str,
    parent_conversation_id: Option<&str>,
    agent_config_id: Option<&str>,
) -> Result<String> {
    let id = generate_conversation_id();
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO conversations (id, project_id, parent_conversation_id, title, updated_at, created_at, agent_config_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, project_id, parent_conversation_id, title, now, now, agent_config_id],
    )?;
    Ok(id)
}

/// 按 id 查询对话
pub fn get(conn: &Connection, id: &str) -> Result<Option<ConversationRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, parent_conversation_id, title, updated_at, created_at, agent_config_id, status
         FROM conversations WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        let updated_str: String = row.get(4)?;
        let created_str: String = row.get(5)?;
        Ok(ConversationRow {
            id: row.get(0)?,
            project_id: row.get(1)?,
            parent_conversation_id: row.get(2)?,
            title: row.get(3)?,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Local))
                .unwrap_or_else(|_| Local::now()),
            updated_at: DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Local))
                .unwrap_or_else(|_| Local::now()),
            message_count: 0,
            agent_config_id: row.get(6)?,
            status: ConversationStatus::from_str(&row.get::<_, String>(7)?),
        })
    })?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 列出某项目下所有活跃对话，按 updated_at 降序
pub fn list_by_project(conn: &Connection, project_id: &str) -> Result<Vec<ConversationRow>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.project_id, c.parent_conversation_id, c.title, c.updated_at, c.created_at, c.agent_config_id, c.status,
                (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND active = 'active') AS msg_count
         FROM conversations c WHERE c.project_id = ?1 AND c.status = 'active' AND c.parent_conversation_id IS NULL
         ORDER BY c.updated_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        let updated_str: String = row.get(4)?;
        let created_str: String = row.get(5)?;
        Ok(ConversationRow {
            id: row.get(0)?,
            project_id: row.get(1)?,
            parent_conversation_id: row.get(2)?,
            title: row.get(3)?,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .map(|dt| dt.with_timezone(&Local))
                .unwrap_or_else(|_| Local::now()),
            updated_at: DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Local))
                .unwrap_or_else(|_| Local::now()),
            message_count: row.get(8)?,
            agent_config_id: row.get(6)?,
            status: ConversationStatus::from_str(&row.get::<_, String>(7)?),
        })
    })?;
    rows.collect()
}

/// 更新对话标题和 updated_at（不改变 agent_config_id）
pub fn update(conn: &Connection, row: &ConversationRow) -> Result<()> {
    conn.execute(
        "UPDATE conversations SET title=?1, updated_at=?2 WHERE id=?3",
        params![row.title, row.updated_at.to_rfc3339(), row.id],
    )?;
    Ok(())
}

/// 软删除对话
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("UPDATE conversations SET status='deleted' WHERE id=?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::metadata::project;

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
                parent_conversation_id TEXT REFERENCES conversations(id),
                title TEXT DEFAULT '', updated_at TEXT NOT NULL,
                created_at TEXT NOT NULL,
                agent_config_id TEXT,
                status TEXT NOT NULL DEFAULT 'active'
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id),
                role TEXT NOT NULL, content TEXT, timestamp TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    fn test_conn_with_agent_configs() -> (Connection, String) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE agent_configs (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, agent_type TEXT DEFAULT 'general',
                provider_instance_id TEXT DEFAULT '', model TEXT DEFAULT '',
                base_url TEXT DEFAULT '', api_key TEXT DEFAULT '',
                system_prompt TEXT DEFAULT '', temperature REAL DEFAULT 0.7,
                max_tokens INTEGER, tools TEXT DEFAULT '[]',
                created_at TEXT NOT NULL, updated_at TEXT NOT NULL
            );
            CREATE TABLE projects (
                id TEXT PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL,
                created_at TEXT NOT NULL, indicator_color TEXT DEFAULT '',
                last_activity_at TEXT
            );
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL REFERENCES projects(id),
                parent_conversation_id TEXT REFERENCES conversations(id),
                title TEXT DEFAULT '', updated_at TEXT NOT NULL,
                created_at TEXT NOT NULL,
                agent_config_id TEXT REFERENCES agent_configs(id),
                status TEXT NOT NULL DEFAULT 'active'
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id),
                role TEXT NOT NULL, content TEXT, timestamp TEXT NOT NULL
            );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO agent_configs (id, name, created_at, updated_at) VALUES ('acfg-1', 'general', 'now', 'now')",
            [],
        ).unwrap();
        let pid = project::create(&conn, "p", "/p").unwrap();
        (conn, pid)
    }

    #[test]
    fn test_create_and_get() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let id = create(&conn, &pid, "hello", None, None).unwrap();
        assert!(id.starts_with("conv-"));

        let row = get(&conn, &id).unwrap().expect("should exist");
        assert_eq!(row.project_id, pid);
        assert_eq!(row.title, "hello");
        assert!(row.agent_config_id.is_none());
    }

    #[test]
    fn test_create_with_agent_config_id() {
        let (conn, pid) = test_conn_with_agent_configs();
        let id = create(&conn, &pid, "with config", None, Some("acfg-1")).unwrap();
        let row = get(&conn, &id).unwrap().expect("should exist");
        assert_eq!(row.agent_config_id, Some("acfg-1".to_string()));
    }

    #[test]
    fn test_list_by_project() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        create(&conn, &pid, "a", None, None).unwrap();
        create(&conn, &pid, "b", None, None).unwrap();
        let rows = list_by_project(&conn, &pid).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_update() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let id = create(&conn, &pid, "old", None, None).unwrap();
        let mut row = get(&conn, &id).unwrap().unwrap();
        row.title = "new".into();
        update(&conn, &row).unwrap();
        let updated = get(&conn, &id).unwrap().unwrap();
        assert_eq!(updated.title, "new");
    }

    #[test]
    fn test_delete() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        let id = create(&conn, &pid, "x", None, None).unwrap();
        delete(&conn, &id).unwrap();
        assert!(get(&conn, &id).unwrap().is_none());
    }
}