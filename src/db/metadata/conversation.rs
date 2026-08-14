// ── Conversation CRUD ──

use chrono::{DateTime, Local};
use rusqlite::{Connection, Result, params};

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
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_reasoning_tokens: i64,
    pub total_tokens: i64,
    pub request_count: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
}

/// 看板查询结果 —— 对话 + token 统计摘要
#[derive(Debug, Clone, PartialEq)]
pub struct ConversationWithUsage {
    pub id: String,
    pub title: String,
    pub status: ConversationStatus,
    pub parent_conversation_id: Option<String>,
    pub updated_at: DateTime<Local>,
    pub message_count: i64,
    pub total_prompt_tokens: i64,
    pub total_completion_tokens: i64,
    pub total_reasoning_tokens: i64,
    pub total_tokens: i64,
    pub request_count: i64,
    pub cache_hit_tokens: i64,
    pub cache_miss_tokens: i64,
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
    create_with_status(
        conn,
        project_id,
        title,
        parent_conversation_id,
        agent_config_id,
        ConversationStatus::Active,
    )
}

/// 创建指定状态的对话，返回新 id（书聊等隐藏对话用 SubAgent）。
pub fn create_with_status(
    conn: &Connection,
    project_id: &str,
    title: &str,
    parent_conversation_id: Option<&str>,
    agent_config_id: Option<&str>,
    status: ConversationStatus,
) -> Result<String> {
    let id = generate_conversation_id();
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO conversations (id, project_id, parent_conversation_id, title, updated_at, created_at, agent_config_id, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            id,
            project_id,
            parent_conversation_id,
            title,
            now,
            now,
            agent_config_id,
            status.as_str()
        ],
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
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_reasoning_tokens: 0,
            total_tokens: 0,
            request_count: 0,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
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
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_reasoning_tokens: 0,
            total_tokens: 0,
            request_count: 0,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
        })
    })?;
    rows.collect()
}

/// 列出某项目下指定 agent 配置的隐藏对话（书聊/阅读对话，status='sub_agent'）。
pub fn list_by_agent_config(
    conn: &Connection,
    project_id: &str,
    agent_config_id: &str,
) -> Result<Vec<ConversationRow>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.project_id, c.parent_conversation_id, c.title, c.updated_at, c.created_at, c.agent_config_id, c.status,
                (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND active = 'active') AS msg_count
         FROM conversations c
         WHERE c.project_id = ?1 AND c.status = 'sub_agent' AND c.agent_config_id = ?2
         ORDER BY c.updated_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id, agent_config_id], |row| {
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
            total_prompt_tokens: 0,
            total_completion_tokens: 0,
            total_reasoning_tokens: 0,
            total_tokens: 0,
            request_count: 0,
            cache_hit_tokens: 0,
            cache_miss_tokens: 0,
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
    conn.execute(
        "UPDATE conversations SET status='deleted' WHERE id=?1",
        params![id],
    )?;
    Ok(())
}

/// 累加 token 用量到对话记录（每次 LLM 交互完成后调用）
pub fn accumulate_usage(
    conn: &Connection,
    conv_id: &str,
    usage: &crate::model::TokenUsageRecord,
) -> Result<()> {
    let now = chrono::Local::now().to_rfc3339();
    tracing::debug!(target: "dashboard", conv_id = %conv_id, prompt = usage.prompt_tokens, completion = usage.completion_tokens, reasoning = usage.reasoning_tokens, total = usage.total_tokens, "accumulate usage");
    conn.execute(
        "UPDATE conversations SET
            total_prompt_tokens = total_prompt_tokens + ?1,
            total_completion_tokens = total_completion_tokens + ?2,
            total_reasoning_tokens = total_reasoning_tokens + ?3,
            total_tokens = total_tokens + ?4,
            request_count = request_count + 1,
            cache_hit_tokens = cache_hit_tokens + ?5,
            cache_miss_tokens = cache_miss_tokens + ?6,
            last_usage_at = ?7
         WHERE id = ?8",
        params![
            usage.prompt_tokens as i64,
            usage.completion_tokens as i64,
            usage.reasoning_tokens as i64,
            usage.total_tokens as i64,
            usage.cache_hit_tokens as i64,
            usage.cache_miss_tokens as i64,
            now,
            conv_id,
        ],
    )?;
    tracing::debug!(target: "dashboard", conv_id = %conv_id, total = usage.total_tokens, "accumulate usage persisted");
    Ok(())
}

/// 读取某对话的累计 token 用量（用于加载已有对话时恢复 runtime 状态）
pub fn get_usage(conn: &Connection, conv_id: &str) -> Result<crate::model::TokenUsageRecord> {
    let mut stmt = conn.prepare(
        "SELECT total_prompt_tokens, total_completion_tokens, total_reasoning_tokens,
                total_tokens, cache_hit_tokens, cache_miss_tokens, request_count
         FROM conversations WHERE id = ?1",
    )?;
    let row = stmt.query_row(params![conv_id], |row| {
        Ok(crate::model::TokenUsageRecord {
            prompt_tokens: row.get::<_, i64>(0)? as u32,
            completion_tokens: row.get::<_, i64>(1)? as u32,
            reasoning_tokens: row.get::<_, i64>(2)? as u32,
            total_tokens: row.get::<_, i64>(3)? as u32,
            cache_hit_tokens: row.get::<_, i64>(4)? as u32,
            cache_miss_tokens: row.get::<_, i64>(5)? as u32,
        })
    })?;
    Ok(row)
}

/// 读取某对话的 request_count
pub fn get_request_count(conn: &Connection, conv_id: &str) -> Result<u64> {
    let mut stmt = conn.prepare("SELECT request_count FROM conversations WHERE id = ?1")?;
    Ok(stmt.query_row(params![conv_id], |row| row.get::<_, i64>(0))? as u64)
}

/// 列出某项目下所有对话（含子 Agent）及其 token 统计，供看板使用
pub fn list_with_usage(conn: &Connection, project_id: &str) -> Result<Vec<ConversationWithUsage>> {
    let mut stmt = conn.prepare(
        "SELECT c.id, c.title, c.status, c.parent_conversation_id, c.updated_at,
                c.total_prompt_tokens, c.total_completion_tokens, c.total_reasoning_tokens,
                c.total_tokens, c.request_count, c.cache_hit_tokens, c.cache_miss_tokens,
                (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND active = 'active') AS msg_count
         FROM conversations c
         WHERE c.project_id = ?1 AND c.status != 'deleted'
         ORDER BY c.updated_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id], |row| {
        let updated_str: String = row.get(4)?;
        Ok(ConversationWithUsage {
            id: row.get(0)?,
            title: row.get(1)?,
            status: ConversationStatus::from_str(&row.get::<_, String>(2)?),
            parent_conversation_id: row.get(3)?,
            updated_at: DateTime::parse_from_rfc3339(&updated_str)
                .map(|dt| dt.with_timezone(&Local))
                .unwrap_or_else(|_| Local::now()),
            total_prompt_tokens: row.get(5)?,
            total_completion_tokens: row.get(6)?,
            total_reasoning_tokens: row.get(7)?,
            total_tokens: row.get(8)?,
            request_count: row.get(9)?,
            cache_hit_tokens: row.get(10)?,
            cache_miss_tokens: row.get(11)?,
            message_count: row.get(12)?,
        })
    })?;
    rows.collect()
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
                status TEXT NOT NULL DEFAULT 'active',
                total_prompt_tokens INTEGER NOT NULL DEFAULT 0,
                total_completion_tokens INTEGER NOT NULL DEFAULT 0,
                total_reasoning_tokens INTEGER NOT NULL DEFAULT 0,
                total_tokens INTEGER NOT NULL DEFAULT 0,
                request_count INTEGER NOT NULL DEFAULT 0,
                cache_hit_tokens INTEGER NOT NULL DEFAULT 0,
                cache_miss_tokens INTEGER NOT NULL DEFAULT 0,
                last_usage_at TEXT
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id),
                role TEXT NOT NULL, content TEXT, timestamp TEXT NOT NULL,
                active TEXT NOT NULL DEFAULT 'active'
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
                role TEXT NOT NULL, content TEXT, timestamp TEXT NOT NULL,
                active TEXT NOT NULL DEFAULT 'active'
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
    fn sub_agent_conversations_hidden_from_project_list() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        create(&conn, &pid, "visible", None, None).unwrap();
        create_with_status(
            &conn,
            &pid,
            "hidden book chat",
            None,
            None,
            ConversationStatus::SubAgent,
        )
        .unwrap();
        let rows = list_by_project(&conn, &pid).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "visible");
    }

    #[test]
    fn list_by_agent_config_returns_sub_agent_reading_conversations() {
        let conn = test_conn();
        let pid = project::create(&conn, "p", "/p").unwrap();
        create_with_status(
            &conn,
            &pid,
            "阅读对话 1",
            None,
            Some("acfg-read-helper"),
            ConversationStatus::SubAgent,
        )
        .unwrap();
        create_with_status(
            &conn,
            &pid,
            "普通对话",
            None,
            Some("acfg-read-helper"),
            ConversationStatus::Active,
        )
        .unwrap();
        let rows = list_by_agent_config(&conn, &pid, "acfg-read-helper").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].title, "阅读对话 1");
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
        // 软删除：get 仍返回行，但 status 为 deleted
        let row = get(&conn, &id)
            .unwrap()
            .expect("row should still exist after soft delete");
        assert_eq!(row.status, ConversationStatus::Deleted);
    }
}
