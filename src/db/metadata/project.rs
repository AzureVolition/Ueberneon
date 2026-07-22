// ── Project CRUD ──

use chrono::{DateTime, Local};
use rusqlite::{params, Connection, Result};

/// 数据库行 —— 不含嵌套的 conversations 列表
#[derive(Debug, Clone)]
pub struct ProjectRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: DateTime<Local>,
    pub indicator_color: String,
    pub last_activity_at: Option<DateTime<Local>>,
}

use std::sync::atomic::AtomicU16;

static ID_COUNTER: AtomicU16 = AtomicU16::new(0);

/// 生成形如 `proj-1748612345678-xxxx` 的 id
fn generate_id() -> String {
    let millis = chrono::Local::now().timestamp_millis();
    let seq = ID_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % 9999;
    format!("proj-{millis}-{seq}")
}

/// 创建项目，返回新 id
pub fn create(
    conn: &Connection,
    name: &str,
    path: &str,
) -> Result<String> {
    let id = generate_id();
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO projects (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, name, path, now],
    )?;
    Ok(id)
}

/// 按 id 查询项目
pub fn get(conn: &Connection, id: &str) -> Result<Option<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, path, created_at, indicator_color, last_activity_at
         FROM projects WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        let created_at_str: String = row.get(3)?;
        let last_str: Option<String> = row.get(5)?;
        Ok(ProjectRow {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            created_at: DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&Local))
                .unwrap_or_else(|_| Local::now()),
            indicator_color: row.get(4)?,
            last_activity_at: last_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Local)),
        })
    })?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 列出所有项目
pub fn list(conn: &Connection) -> Result<Vec<ProjectRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, path, created_at, indicator_color, last_activity_at
         FROM projects ORDER BY created_at",
    )?;
    let rows = stmt.query_map([], |row| {
        let created_at_str: String = row.get(3)?;
        let last_str: Option<String> = row.get(5)?;
        Ok(ProjectRow {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
            created_at: DateTime::parse_from_rfc3339(&created_at_str)
                .map(|dt| dt.with_timezone(&Local))
                .unwrap_or_else(|_| Local::now()),
            indicator_color: row.get(4)?,
            last_activity_at: last_str
                .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                .map(|dt| dt.with_timezone(&Local)),
        })
    })?;
    rows.collect()
}

/// 更新项目（全部字段覆盖）
pub fn update(conn: &Connection, row: &ProjectRow) -> Result<()> {
    conn.execute(
        "UPDATE projects SET name=?1, path=?2, indicator_color=?3, last_activity_at=?4
         WHERE id=?5",
        params![
            row.name,
            row.path,
            row.indicator_color,
            row.last_activity_at.map(|t| t.to_rfc3339()),
            row.id,
        ],
    )?;
    Ok(())
}

/// 更新项目 last_activity_at 为当前时间
pub fn touch(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE projects SET last_activity_at = ?1 WHERE id = ?2",
        params![chrono::Local::now().to_rfc3339(), id],
    )?;
    Ok(())
}

/// 删除项目
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM projects WHERE id=?1", params![id])?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE projects (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                created_at TEXT NOT NULL,
                indicator_color TEXT DEFAULT '',
                last_activity_at TEXT
            );",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_create_and_get() {
        let conn = test_conn();
        let id = create(&conn, "test-proj", "/tmp/test").unwrap();
        assert!(id.starts_with("proj-"));

        let row = get(&conn, &id).unwrap().expect("should exist");
        assert_eq!(row.name, "test-proj");
        assert_eq!(row.path, "/tmp/test");
    }

    #[test]
    fn test_list() {
        let conn = test_conn();
        create(&conn, "a", "/a").unwrap();
        create(&conn, "b", "/b").unwrap();
        let rows = list(&conn).unwrap();
        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn test_update() {
        let conn = test_conn();
        let id = create(&conn, "old", "/old").unwrap();
        let mut row = get(&conn, &id).unwrap().unwrap();
        row.name = "new".into();
        update(&conn, &row).unwrap();
        let updated = get(&conn, &id).unwrap().unwrap();
        assert_eq!(updated.name, "new");
    }

    #[test]
    fn test_delete() {
        let conn = test_conn();
        let id = create(&conn, "x", "/x").unwrap();
        delete(&conn, &id).unwrap();
        assert!(get(&conn, &id).unwrap().is_none());
    }
}
