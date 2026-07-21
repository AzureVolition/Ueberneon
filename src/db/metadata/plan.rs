// ── Plan CRUD ──

use rusqlite::{params, Connection, Result};

/// 计划状态枚举（对应数据库 plans.status 的小写字符串）
#[derive(Debug, Clone, PartialEq)]
pub enum PlanStatus {
    NeedApproval,
    InProgress,
    Completed,
    Canceled,
}

impl PlanStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            PlanStatus::NeedApproval => "need_approval",
            PlanStatus::InProgress => "in_progress",
            PlanStatus::Completed => "completed",
            PlanStatus::Canceled => "canceled",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "need_approval" => Some(PlanStatus::NeedApproval),
            "in_progress" => Some(PlanStatus::InProgress),
            "completed" => Some(PlanStatus::Completed),
            "canceled" => Some(PlanStatus::Canceled),
            _ => None,
        }
    }
}

/// 数据库行
#[derive(Debug, Clone)]
pub struct PlanRow {
    pub id: String,
    pub project_id: String,
    pub conversation_id: String,
    pub goal: String,
    pub description: String,
    pub status: PlanStatus,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub created_at: String,
}

use std::sync::atomic::AtomicU16;
static ID_COUNTER: AtomicU16 = AtomicU16::new(0);

fn generate_id() -> String {
    let millis = chrono::Local::now().timestamp_millis();
    let seq = ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 9999;
    format!("plan-{millis}-{seq}")
}

/// 创建计划，返回新 id
pub fn create(
    conn: &Connection,
    project_id: &str,
    conversation_id: &str,
    goal: &str,
    description: &str,
    status: PlanStatus,
) -> Result<String> {
    let id = generate_id();
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO plans (id, project_id, conversation_id, goal, description, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![id, project_id, conversation_id, goal, description, status.as_str(), now],
    )?;
    Ok(id)
}

fn row_to_plan(row: &rusqlite::Row) -> Result<PlanRow> {
    let status_str: String = row.get(5)?;
    Ok(PlanRow {
        id: row.get(0)?,
        project_id: row.get(1)?,
        conversation_id: row.get(2)?,
        goal: row.get(3)?,
        description: row.get(4)?,
        status: PlanStatus::from_str(&status_str).unwrap_or(PlanStatus::NeedApproval),
        started_at: row.get(6)?,
        completed_at: row.get(7)?,
        created_at: row.get(8)?,
    })
}

/// 按 id 查询
pub fn get(conn: &Connection, id: &str) -> Result<Option<PlanRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, conversation_id, goal, description, status, started_at, completed_at, created_at
         FROM plans WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], row_to_plan)?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 按项目列出所有计划（最新优先）
pub fn list_by_project(conn: &Connection, project_id: &str) -> Result<Vec<PlanRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, project_id, conversation_id, goal, description, status, started_at, completed_at, created_at
         FROM plans WHERE project_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id], row_to_plan)?;
    rows.collect()
}

/// 更新计划状态
pub fn update_status(conn: &Connection, id: &str, status: PlanStatus) -> Result<()> {
    conn.execute(
        "UPDATE plans SET status = ?1 WHERE id = ?2",
        params![status.as_str(), id],
    )?;
    Ok(())
}

/// 标记开始
pub fn mark_started(conn: &Connection, id: &str) -> Result<()> {
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "UPDATE plans SET status = ?1, started_at = ?2 WHERE id = ?3",
        params![PlanStatus::InProgress.as_str(), now, id],
    )?;
    Ok(())
}

/// 标记完成
pub fn mark_completed(conn: &Connection, id: &str) -> Result<()> {
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "UPDATE plans SET status = ?1, completed_at = ?2 WHERE id = ?3",
        params![PlanStatus::Completed.as_str(), now, id],
    )?;
    Ok(())
}

/// 删除计划
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM plans WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE plans (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL,
                conversation_id TEXT NOT NULL,
                goal TEXT NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                started_at TEXT,
                completed_at TEXT,
                created_at TEXT NOT NULL
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_create_and_get() {
        let conn = test_conn();
        let id = create(&conn, "proj-1", "conv-1", "test goal", "", PlanStatus::InProgress).unwrap();
        assert!(id.starts_with("plan-"));

        let row = get(&conn, &id).unwrap().expect("should exist");
        assert_eq!(row.goal, "test goal");
        assert_eq!(row.status, PlanStatus::InProgress);
    }

    #[test]
    fn test_list_by_project() {
        let conn = test_conn();
        create(&conn, "proj-1", "c1", "goal a", "", PlanStatus::InProgress).unwrap();
        create(&conn, "proj-1", "c2", "goal b", "", PlanStatus::Completed).unwrap();
        let rows = list_by_project(&conn, "proj-1").unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_update_status() {
        let conn = test_conn();
        let id = create(&conn, "p1", "c1", "goal", "", PlanStatus::NeedApproval).unwrap();
        update_status(&conn, &id, PlanStatus::Completed).unwrap();
        let row = get(&conn, &id).unwrap().unwrap();
        assert_eq!(row.status, PlanStatus::Completed);
    }

    #[test]
    fn test_mark_started() {
        let conn = test_conn();
        let id = create(&conn, "p1", "c1", "goal", "", PlanStatus::NeedApproval).unwrap();
        mark_started(&conn, &id).unwrap();
        let row = get(&conn, &id).unwrap().unwrap();
        assert_eq!(row.status, PlanStatus::InProgress);
        assert!(row.started_at.is_some());
    }

    #[test]
    fn test_mark_completed() {
        let conn = test_conn();
        let id = create(&conn, "p1", "c1", "goal", "", PlanStatus::InProgress).unwrap();
        mark_completed(&conn, &id).unwrap();
        let row = get(&conn, &id).unwrap().unwrap();
        assert_eq!(row.status, PlanStatus::Completed);
        assert!(row.completed_at.is_some());
    }

    #[test]
    fn test_delete() {
        let conn = test_conn();
        let id = create(&conn, "p1", "c1", "goal", "", PlanStatus::InProgress).unwrap();
        delete(&conn, &id).unwrap();
        assert!(get(&conn, &id).unwrap().is_none());
    }
}
