// ── Project CRUD ──

use chrono::{DateTime, Local};
use rusqlite::{Connection, Result, params};
use std::path::{Path, PathBuf};

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
pub fn create(conn: &Connection, name: &str, path: &str) -> Result<String> {
    let id = generate_id();
    let now = chrono::Local::now().to_rfc3339();
    conn.execute(
        "INSERT INTO projects (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, name, path, now],
    )?;
    Ok(id)
}

/// 创建应用管理的项目：名称 trim 后不能为空且不能与现有项目重名，
/// 自动在项目根目录下创建 `note/` 文件夹，再写入 projects 表。
pub fn create_managed(conn: &Connection, name: &str) -> anyhow::Result<String> {
    create_managed_at(conn, name, &crate::layout::projects_root())
}

/// 创建入口（测试可注入项目根目录）
fn create_managed_at(conn: &Connection, name: &str, root: &Path) -> anyhow::Result<String> {
    use anyhow::{Context, anyhow};

    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("project name cannot be empty"));
    }
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM projects WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    if exists > 0 {
        return Err(anyhow!("project name already exists: {name}"));
    }

    let existing_paths: Vec<PathBuf> = {
        let mut stmt = conn.prepare("SELECT path FROM projects")?;
        stmt.query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .map(PathBuf::from)
            .collect()
    };
    let dir = crate::layout::unique_project_dir(root, name, &existing_paths);
    std::fs::create_dir_all(crate::layout::project_note_dir(&dir))
        .with_context(|| format!("failed to create project dir: {}", dir.display()))?;
    let id = create(conn, name, &dir.display().to_string())?;
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

/// 删除项目及其全部关联数据（消息/对话/计划/任务/书关联）。
/// 若项目目录位于应用数据目录内（~/.ueberneon/projects/），一并移除目录；
/// 外部路径项目只删应用记录，不触碰磁盘。
pub fn delete_full(conn: &Connection, id: &str) -> anyhow::Result<()> {
    delete_full_at(conn, id, &crate::layout::projects_root())
}

/// 删除入口（测试可注入项目根目录）
fn delete_full_at(conn: &Connection, id: &str, root: &Path) -> anyhow::Result<()> {
    let project = get(conn, id)?;
    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM tasks WHERE project_id = ?1", params![id])?;
    tx.execute("DELETE FROM plans WHERE project_id = ?1", params![id])?;
    tx.execute(
        "DELETE FROM messages WHERE conversation_id IN
         (SELECT id FROM conversations WHERE project_id = ?1)",
        params![id],
    )?;
    tx.execute(
        "DELETE FROM conversations WHERE project_id = ?1",
        params![id],
    )?;
    tx.execute(
        "DELETE FROM project_books WHERE project_id = ?1",
        params![id],
    )?;
    tx.execute("DELETE FROM projects WHERE id = ?1", params![id])?;
    tx.commit()?;

    if let Some(row) = project {
        let path = PathBuf::from(&row.path);
        if path.starts_with(&root) && path != root {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(
                    target: "db",
                    error = %e,
                    path = %path.display(),
                    "failed to remove project dir"
                );
            }
        }
    }
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
            );
            CREATE TABLE conversations (
                id          TEXT PRIMARY KEY,
                project_id  TEXT NOT NULL REFERENCES projects(id),
                parent_conversation_id TEXT REFERENCES conversations(id),
                title       TEXT DEFAULT '',
                updated_at  TEXT NOT NULL,
                created_at  TEXT NOT NULL,
                agent_config_id TEXT,
                status      TEXT NOT NULL DEFAULT 'active'
            );
            CREATE TABLE messages (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id     TEXT NOT NULL REFERENCES conversations(id),
                role                TEXT NOT NULL,
                content             TEXT,
                timestamp           TEXT NOT NULL
            );
            CREATE TABLE plans (
                id              TEXT PRIMARY KEY,
                project_id      TEXT NOT NULL REFERENCES projects(id),
                conversation_id TEXT NOT NULL REFERENCES conversations(id),
                goal            TEXT NOT NULL,
                description     TEXT NOT NULL DEFAULT '',
                status          TEXT NOT NULL DEFAULT 'need_approval',
                started_at      TEXT,
                completed_at    TEXT,
                created_at      TEXT NOT NULL
            );
            CREATE TABLE tasks (
                id              INTEGER PRIMARY KEY AUTOINCREMENT,
                plan_id         TEXT NOT NULL REFERENCES plans(id),
                project_id      TEXT NOT NULL REFERENCES projects(id),
                parent_task_id  INTEGER REFERENCES tasks(id),
                idx             INTEGER NOT NULL,
                description     TEXT NOT NULL,
                status          TEXT NOT NULL DEFAULT 'pending',
                tool_hint       TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );
            CREATE TABLE project_books (
                project_id      TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                book_id         TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
                created_at      TEXT NOT NULL,
                PRIMARY KEY (project_id, book_id)
            );
            CREATE TABLE books (
                id              TEXT PRIMARY KEY,
                name            TEXT NOT NULL UNIQUE,
                path            TEXT NOT NULL,
                created_at      TEXT NOT NULL
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

    #[test]
    fn test_create_managed_creates_note_dir() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-project-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let id = create_managed_at(&conn, "线性代数", &root).unwrap();
        let row = get(&conn, &id).unwrap().expect("should exist");
        let dir = std::path::PathBuf::from(&row.path);
        assert!(dir.join("note").is_dir(), "note dir should exist");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_create_managed_rejects_duplicate_name() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-project-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        create_managed_at(&conn, "math", &root).unwrap();
        let err = create_managed_at(&conn, "  math  ", &root).unwrap_err();
        assert!(err.to_string().contains("already exists"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_create_managed_rejects_empty_name() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-project-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let err = create_managed_at(&conn, "   ", &root).unwrap_err();
        assert!(err.to_string().contains("cannot be empty"));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_delete_full_removes_managed_dir() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-project-del-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);

        let id = create_managed_at(&conn, "to-delete", &root).unwrap();
        let row = get(&conn, &id).unwrap().unwrap();
        let dir = std::path::PathBuf::from(&row.path);
        assert!(dir.is_dir());

        delete_full_at(&conn, &id, &root).unwrap();
        assert!(get(&conn, &id).unwrap().is_none());
        assert!(!dir.exists(), "managed project dir should be removed");

        let _ = std::fs::remove_dir_all(&root);
    }
}
