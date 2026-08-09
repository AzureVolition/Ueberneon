// ── 全局书库 ──
//
// 书库以磁盘为真相：~/.ueberneon/books/<书目录>/ 下的目录在启动时同步进 books 表。
// 项目通过 project_books 多对多关联「引入」书，并在 <项目>/books/<书名> 建符号链接，
// 使 Agent 在项目工作区内即可读取书的内容；书的内容始终只存在全局书库，不复制进项目。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};

use crate::layout;

/// books 表行
#[derive(Debug, Clone)]
pub struct BookRow {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
}

static ID_COUNTER: AtomicU16 = AtomicU16::new(0);

/// 生成形如 `book-<millis>-<seq>` 的 id
fn generate_id() -> String {
    let millis = chrono::Local::now().timestamp_millis();
    let seq = ID_COUNTER.fetch_add(1, Ordering::Relaxed) % 9999;
    format!("book-{millis}-{seq}")
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

fn row_to_book(row: &rusqlite::Row<'_>) -> rusqlite::Result<BookRow> {
    Ok(BookRow {
        id: row.get(0)?,
        name: row.get(1)?,
        path: row.get(2)?,
        created_at: row.get(3)?,
    })
}

/// 列出全局书库全部书
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<BookRow>> {
    let mut stmt = conn.prepare("SELECT id, name, path, created_at FROM books ORDER BY name")?;
    let rows = stmt.query_map([], row_to_book)?;
    rows.collect()
}

/// 按 id 查询一本书
pub fn get(conn: &Connection, id: &str) -> rusqlite::Result<Option<BookRow>> {
    let mut stmt = conn.prepare("SELECT id, name, path, created_at FROM books WHERE id = ?1")?;
    let mut rows = stmt.query_map(params![id], row_to_book)?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 某项目已引入的书
pub fn list_by_project(conn: &Connection, project_id: &str) -> rusqlite::Result<Vec<BookRow>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.name, b.path, b.created_at
         FROM books b
         JOIN project_books pb ON pb.book_id = b.id
         WHERE pb.project_id = ?1
         ORDER BY b.name",
    )?;
    let rows = stmt.query_map(params![project_id], row_to_book)?;
    rows.collect()
}

/// 某项目已引入书的 id 集合
pub fn project_book_ids(conn: &Connection, project_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT book_id FROM project_books WHERE project_id = ?1")?;
    let rows = stmt.query_map(params![project_id], |r| r.get(0))?;
    rows.collect()
}

/// 把 ~/.ueberneon/books/ 下的书同步进 books 表（只增不改，不删记录），
/// 并补齐各项目已引入书的符号链接。
pub fn sync_from_disk(conn: &Connection) -> Result<()> {
    let root = layout::books_root();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create books dir: {}", root.display()))?;
    sync_from_disk_with_root(conn, &root)
}

/// 同步入口（测试可注入根目录）
fn sync_from_disk_with_root(conn: &Connection, root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root)
        .with_context(|| format!("failed to read books dir: {}", root.display()))?
    {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue;
        }
        let path = entry.path();
        conn.execute(
            "INSERT OR IGNORE INTO books (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![
                generate_id(),
                name,
                path.display().to_string(),
                now_rfc3339()
            ],
        )
        .context("insert book row failed")?;
        conn.execute(
            "UPDATE books SET path = ?1 WHERE name = ?2",
            params![path.display().to_string(), name],
        )
        .context("update book path failed")?;
    }

    sync_all_project_links(conn)?;
    Ok(())
}

/// 项目引入书：写入关联 + 建符号链接
pub fn add_to_project(conn: &Connection, project_id: &str, book_id: &str) -> Result<(), String> {
    let project = crate::db::metadata::project::get(conn, project_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "project not found".to_string())?;
    let book = get(conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "book not found".to_string())?;

    conn.execute(
        "INSERT OR IGNORE INTO project_books (project_id, book_id, created_at)
         VALUES (?1, ?2, ?3)",
        params![project_id, book_id, now_rfc3339()],
    )
    .map_err(|e| e.to_string())?;

    link_book(Path::new(&project.path), &book).map_err(|e| e.to_string())
}

/// 项目移除书：删除关联 + 移除符号链接
pub fn remove_from_project(
    conn: &Connection,
    project_id: &str,
    book_id: &str,
) -> Result<(), String> {
    let project = crate::db::metadata::project::get(conn, project_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "project not found".to_string())?;
    let book = get(conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "book not found".to_string())?;

    conn.execute(
        "DELETE FROM project_books WHERE project_id = ?1 AND book_id = ?2",
        params![project_id, book_id],
    )
    .map_err(|e| e.to_string())?;

    let link = layout::project_books_dir(Path::new(&project.path)).join(&book.name);
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        if meta.file_type().is_symlink() {
            std::fs::remove_file(&link)
                .map_err(|e| format!("failed to remove book link {}: {e}", link.display()))?;
        }
    }
    Ok(())
}

/// 为所有项目的已引入书补齐缺失的符号链接（启动时调用，幂等）
fn sync_all_project_links(conn: &Connection) -> Result<()> {
    let projects = crate::db::metadata::project::list(conn)?;
    for project in &projects {
        let book_ids = project_book_ids(conn, &project.id)?;
        for book_id in book_ids {
            let Some(book) = get(conn, &book_id)? else {
                continue;
            };
            if let Err(e) = link_book(Path::new(&project.path), &book) {
                tracing::warn!(
                    target: "books",
                    project = %project.id,
                    book = %book.name,
                    error = %e,
                    "failed to link book into project"
                );
            }
        }
    }
    Ok(())
}

/// 在 <项目>/books/<书名> 建符号链接指向全局书库中的书
fn link_book(project_dir: &Path, book: &BookRow) -> Result<(), String> {
    let books_dir = layout::project_books_dir(project_dir);
    std::fs::create_dir_all(&books_dir)
        .map_err(|e| format!("failed to create {}: {e}", books_dir.display()))?;

    let link = books_dir.join(&book.name);
    if let Ok(meta) = std::fs::symlink_metadata(&link) {
        if meta.file_type().is_symlink() {
            return Ok(()); // 已链接
        }
        return Err(format!(
            "cannot link book '{}': '{}' already exists and is not a link",
            book.name,
            link.display()
        ));
    }

    let target = PathBuf::from(&book.path);
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&target, &link).map_err(|e| {
            format!(
                "failed to create link {} -> {}: {e}",
                link.display(),
                target.display()
            )
        })?;
    }
    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_dir(&target, &link).map_err(|e| {
            format!(
                "failed to create link {} -> {}: {e} (symbolic links may require developer mode)",
                link.display(),
                target.display()
            )
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

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
            CREATE TABLE books (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                path TEXT NOT NULL,
                created_at TEXT NOT NULL
            );
            CREATE TABLE project_books (
                project_id TEXT NOT NULL REFERENCES projects(id) ON DELETE CASCADE,
                book_id TEXT NOT NULL REFERENCES books(id) ON DELETE CASCADE,
                created_at TEXT NOT NULL,
                PRIMARY KEY (project_id, book_id)
            );",
        )
        .unwrap();
        conn
    }

    fn insert_project(conn: &Connection, id: &str, path: &str) {
        conn.execute(
            "INSERT INTO projects (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
            params![id, id, path, now_rfc3339()],
        )
        .unwrap();
    }

    #[test]
    fn sync_discovers_books() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("代数")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();

        sync_from_disk_with_root(&conn, &root).unwrap();
        let books = list(&conn).unwrap();
        assert_eq!(books.len(), 1);
        assert_eq!(books[0].name, "代数");
        assert!(books[0].path.ends_with("代数"));

        // 幂等：再次同步不会重复
        sync_from_disk_with_root(&conn, &root).unwrap();
        assert_eq!(list(&conn).unwrap().len(), 1);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn add_remove_creates_and_removes_link() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-link-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("library").join("analysis")).unwrap();
        std::fs::create_dir_all(root.join("projects").join("p1")).unwrap();

        let project_path = root.join("projects").join("p1");
        insert_project(&conn, "p1", project_path.to_str().unwrap());
        sync_from_disk_with_root(&conn, &root.join("library")).unwrap();

        let books = list(&conn).unwrap();
        assert_eq!(books.len(), 1);
        let book_id = books[0].id.clone();

        add_to_project(&conn, "p1", &book_id).unwrap();
        let link = layout::project_books_dir(&project_path).join("analysis");
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        remove_from_project(&conn, "p1", &book_id).unwrap();
        assert!(!link.exists());

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn duplicate_book_name_rejected() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('b1', 'same', '/x', ?1)",
            [now_rfc3339()],
        )
        .unwrap();
        let err = conn
            .execute(
                "INSERT INTO books (id, name, path, created_at) VALUES ('b2', 'same', '/y', ?1)",
                [now_rfc3339()],
            )
            .unwrap_err();
        assert!(err.to_string().contains("UNIQUE"));
    }
}
