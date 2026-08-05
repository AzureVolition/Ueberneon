// ── Task CRUD ──

use rusqlite::{Connection, Result, params};

/// 任务状态枚举（对应数据库 tasks.status 的小写字符串）
#[derive(Debug, Clone, PartialEq)]
pub enum TaskStatus {
    Pending,
    InProgress,
    Completed,
    Blocked,
    Failed,
}

impl TaskStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            TaskStatus::Pending => "pending",
            TaskStatus::InProgress => "in_progress",
            TaskStatus::Completed => "completed",
            TaskStatus::Blocked => "blocked",
            TaskStatus::Failed => "failed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(TaskStatus::Pending),
            "in_progress" => Some(TaskStatus::InProgress),
            "completed" => Some(TaskStatus::Completed),
            "blocked" => Some(TaskStatus::Blocked),
            "failed" => Some(TaskStatus::Failed),
            _ => None,
        }
    }
}

/// 数据库行
#[derive(Debug, Clone)]
pub struct TaskRow {
    pub id: i64,
    pub plan_id: String,
    pub project_id: String,
    pub parent_task_id: Option<i64>,
    pub idx: i32,
    pub description: String,
    pub status: TaskStatus,
    pub tool_hint: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

fn row_to_task(row: &rusqlite::Row) -> Result<TaskRow> {
    let status_str: String = row.get(6)?;
    Ok(TaskRow {
        id: row.get(0)?,
        plan_id: row.get(1)?,
        project_id: row.get(2)?,
        parent_task_id: row.get(3)?,
        idx: row.get(4)?,
        description: row.get(5)?,
        status: TaskStatus::from_str(&status_str).unwrap_or(TaskStatus::Pending),
        tool_hint: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
    })
}

/// 创建任务，返回新 id
pub fn create(
    conn: &Connection,
    plan_id: &str,
    project_id: &str,
    parent_task_id: Option<i64>,
    idx: i32,
    description: &str,
    status: TaskStatus,
    tool_hint: Option<&str>,
) -> Result<i64> {
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO tasks (plan_id, project_id, parent_task_id, idx, description, status, tool_hint, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![plan_id, project_id, parent_task_id, idx, description, status.as_str(), tool_hint, now, now],
    )?;
    Ok(conn.last_insert_rowid())
}

/// 按 id 查询
pub fn get(conn: &Connection, id: i64) -> Result<Option<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, project_id, parent_task_id, idx, description, status, tool_hint, created_at, updated_at
         FROM tasks WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], row_to_task)?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 按计划列出所有任务（按 idx 排序）
pub fn list_by_plan(conn: &Connection, plan_id: &str) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, project_id, parent_task_id, idx, description, status, tool_hint, created_at, updated_at
         FROM tasks WHERE plan_id = ?1 ORDER BY idx",
    )?;
    let rows = stmt.query_map(params![plan_id], row_to_task)?;
    rows.collect()
}

/// 按项目列出所有任务（按创建时间倒序）
pub fn list_by_project(conn: &Connection, project_id: &str) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, project_id, parent_task_id, idx, description, status, tool_hint, created_at, updated_at
         FROM tasks WHERE project_id = ?1 ORDER BY created_at DESC",
    )?;
    let rows = stmt.query_map(params![project_id], row_to_task)?;
    rows.collect()
}

/// 列出指定父任务的所有子任务
pub fn list_children(conn: &Connection, parent_id: i64) -> Result<Vec<TaskRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, plan_id, project_id, parent_task_id, idx, description, status, tool_hint, created_at, updated_at
         FROM tasks WHERE parent_task_id = ?1 ORDER BY idx",
    )?;
    let rows = stmt.query_map(params![parent_id], row_to_task)?;
    rows.collect()
}

/// 更新任务状态
pub fn update_status(conn: &Connection, id: i64, status: TaskStatus) -> Result<()> {
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "UPDATE tasks SET status = ?1, updated_at = ?2 WHERE id = ?3",
        params![status.as_str(), now, id],
    )?;
    Ok(())
}

/// 更新任务描述
pub fn update_description(conn: &Connection, id: i64, description: &str) -> Result<()> {
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "UPDATE tasks SET description = ?1, updated_at = ?2 WHERE id = ?3",
        params![description, now, id],
    )?;
    Ok(())
}

/// 删除任务
pub fn delete(conn: &Connection, id: i64) -> Result<()> {
    conn.execute("DELETE FROM tasks WHERE id = ?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE plans (id TEXT PRIMARY KEY);
             CREATE TABLE tasks (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                plan_id TEXT NOT NULL REFERENCES plans(id),
                project_id TEXT NOT NULL,
                parent_task_id INTEGER REFERENCES tasks(id),
                idx INTEGER NOT NULL,
                description TEXT NOT NULL,
                status TEXT NOT NULL,
                tool_hint TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_create_and_get() {
        let conn = test_conn();
        conn.execute("INSERT INTO plans (id) VALUES ('plan-1')", [])
            .unwrap();

        let id = create(
            &conn,
            "plan-1",
            "proj-1",
            None,
            1,
            "do something",
            TaskStatus::Pending,
            None,
        )
        .unwrap();
        assert!(id > 0);

        let row = get(&conn, id).unwrap().expect("should exist");
        assert_eq!(row.description, "do something");
        assert_eq!(row.status, TaskStatus::Pending);
        assert_eq!(row.project_id, "proj-1");
    }

    #[test]
    fn test_list_by_plan() {
        let conn = test_conn();
        conn.execute("INSERT INTO plans (id) VALUES ('plan-1')", [])
            .unwrap();
        create(
            &conn,
            "plan-1",
            "p1",
            None,
            1,
            "step a",
            TaskStatus::Pending,
            None,
        )
        .unwrap();
        create(
            &conn,
            "plan-1",
            "p1",
            None,
            2,
            "step b",
            TaskStatus::Completed,
            None,
        )
        .unwrap();
        let rows = list_by_plan(&conn, "plan-1").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].status, TaskStatus::Completed);
    }

    #[test]
    fn test_parent_child() {
        let conn = test_conn();
        conn.execute("INSERT INTO plans (id) VALUES ('plan-1')", [])
            .unwrap();
        let parent = create(
            &conn,
            "plan-1",
            "p1",
            None,
            1,
            "parent",
            TaskStatus::InProgress,
            None,
        )
        .unwrap();
        let child = create(
            &conn,
            "plan-1",
            "p1",
            Some(parent),
            1,
            "child",
            TaskStatus::Pending,
            None,
        )
        .unwrap();

        let children = list_children(&conn, parent).unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].id, child);
    }

    #[test]
    fn test_update_status() {
        let conn = test_conn();
        conn.execute("INSERT INTO plans (id) VALUES ('plan-1')", [])
            .unwrap();
        let id = create(
            &conn,
            "plan-1",
            "p1",
            None,
            1,
            "task",
            TaskStatus::Pending,
            None,
        )
        .unwrap();
        update_status(&conn, id, TaskStatus::Completed).unwrap();
        let row = get(&conn, id).unwrap().unwrap();
        assert_eq!(row.status, TaskStatus::Completed);
    }

    #[test]
    fn test_delete() {
        let conn = test_conn();
        conn.execute("INSERT INTO plans (id) VALUES ('plan-1')", [])
            .unwrap();
        let id = create(
            &conn,
            "plan-1",
            "p1",
            None,
            1,
            "task",
            TaskStatus::Pending,
            None,
        )
        .unwrap();
        delete(&conn, id).unwrap();
        assert!(get(&conn, id).unwrap().is_none());
    }
}
