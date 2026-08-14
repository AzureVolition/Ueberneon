// ── 全局书库 ──
//
// 书库以磁盘为真相：~/.ueberneon/books/<书目录>/ 下的目录在启动时同步进 books 表。
// 项目通过 project_books 多对多关联「引入」书；书的内容始终只存在全局书库，
// 不复制进项目。Agent 读取书内容后续通过独立工具按库路径访问。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, Ordering};

use anyhow::{Context, Result, anyhow};
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

/// 引用一本书的项目(仅 id + 名称)
#[derive(Debug, Clone)]
pub struct ProjectRef {
    pub id: String,
    pub name: String,
}

/// 一本书及其被哪些项目引入
#[derive(Debug, Clone)]
pub struct BookWithProjects {
    pub book: BookRow,
    pub projects: Vec<ProjectRef>,
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

/// 列出全部书及其引用项目(书按名称、项目按名称排序;无引用的书 projects 为空)
pub fn list_with_projects(conn: &Connection) -> rusqlite::Result<Vec<BookWithProjects>> {
    let mut stmt = conn.prepare(
        "SELECT b.id, b.name, b.path, b.created_at, p.id, p.name
         FROM books b
         LEFT JOIN project_books pb ON pb.book_id = b.id
         LEFT JOIN projects p ON p.id = pb.project_id
         ORDER BY b.name, p.name",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            BookRow {
                id: r.get(0)?,
                name: r.get(1)?,
                path: r.get(2)?,
                created_at: r.get(3)?,
            },
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
        ))
    })?;

    let mut items: Vec<BookWithProjects> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for row in rows {
        let (book, project_id, project_name) = row?;
        if let Some(&i) = index.get(&book.id) {
            if let (Some(pid), Some(pname)) = (project_id, project_name) {
                items[i].projects.push(ProjectRef {
                    id: pid,
                    name: pname,
                });
            }
        } else {
            let mut projects = Vec::new();
            if let (Some(pid), Some(pname)) = (project_id, project_name) {
                projects.push(ProjectRef {
                    id: pid,
                    name: pname,
                });
            }
            index.insert(book.id.clone(), items.len());
            items.push(BookWithProjects { book, projects });
        }
    }
    Ok(items)
}

/// 某项目已引入书的 id 集合
pub fn project_book_ids(conn: &Connection, project_id: &str) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT book_id FROM project_books WHERE project_id = ?1")?;
    let rows = stmt.query_map(params![project_id], |r| r.get(0))?;
    rows.collect()
}

/// 导入一个 PDF 文件为书：
/// - 书名 = 文件名去扩展名并 trim，与现有 books.name 重名报错；
/// - 创建 ~/.ueberneon/books/<book_id>/ 目录，复制 PDF 为 original.pdf；
/// - 写入 books 表（目录名用 id，避免复杂字符进路径）。
/// 任一步失败会清理已创建的目录，不留半成品。
pub fn import_pdf_file(conn: &Connection, source: &Path) -> Result<String> {
    import_pdf_file_at(conn, source, &layout::books_root())
}

/// 删除一本书:移除项目关联、删除数据库记录,并移除书目录。
/// 书目录不在全局书库根目录内(外部路径)时只删记录,不触碰磁盘。
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    delete_at(conn, id, &layout::books_root())
}

/// 一次性清理旧版本在 <项目>/books/ 下创建的符号链接（迁移 v2 调用）。
/// 只删除链接本身，普通文件/目录不动；链接清空后目录为空则移除目录。
pub(crate) fn cleanup_legacy_book_links(conn: &Connection) -> Result<()> {
    let projects = crate::db::metadata::project::list(conn)?;
    for project in &projects {
        let books_dir = PathBuf::from(&project.path).join("books");
        let Ok(entries) = std::fs::read_dir(&books_dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if meta.file_type().is_symlink() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        tracing::warn!(
                            target: "books",
                            path = %path.display(),
                            error = %e,
                            "failed to remove legacy book link"
                        );
                    }
                }
            }
        }
        let _ = std::fs::remove_dir(&books_dir);
    }
    Ok(())
}

/// 删除入口(测试可注入书库根目录)
fn delete_at(conn: &Connection, id: &str, root: &Path) -> Result<()> {
    let Some(book) = get(conn, id)? else {
        return Err(anyhow!("book not found: {id}"));
    };
    let chat_conv_id = crate::db::metadata::book_chat::get_by_book(conn, id)?
        .map(|r| r.conversation_id);

    let tx = conn.unchecked_transaction()?;
    tx.execute("DELETE FROM project_books WHERE book_id = ?1", params![id])?;
    tx.execute(
        "DELETE FROM messages WHERE conversation_id IN
         (SELECT conversation_id FROM book_chats WHERE book_id = ?1)",
        params![id],
    )?;
    tx.execute(
        "DELETE FROM conversations WHERE id IN
         (SELECT conversation_id FROM book_chats WHERE book_id = ?1)",
        params![id],
    )?;
    tx.execute("DELETE FROM book_chats WHERE book_id = ?1", params![id])?;
    tx.execute("DELETE FROM books WHERE id = ?1", params![id])?;
    tx.commit()?;

    if let Some(cid) = chat_conv_id {
        crate::state_agent::manager::AgentManager::get().remove(&cid);
    }

    // 书目录在应用书库根目录内才删除,外部路径只清记录。
    let dir = PathBuf::from(&book.path);
    if dir.starts_with(root) && dir != root {
        if let Err(e) = std::fs::remove_dir_all(&dir) {
            tracing::warn!(
                target: "books",
                book = %id,
                error = %e,
                path = %dir.display(),
                "failed to remove book dir"
            );
        }
    }
    Ok(())
}

/// 导入入口（测试可注入书库根目录）
fn import_pdf_file_at(conn: &Connection, source: &Path, root: &Path) -> Result<String> {
    let name = source
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("invalid pdf file name: {}", source.display()))?;

    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM books WHERE name = ?1",
        params![name],
        |r| r.get(0),
    )?;
    if exists > 0 {
        return Err(anyhow!("book already exists: {name}"));
    }

    let id = generate_id();
    let dir = root.join(&id);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("failed to create book dir: {}", dir.display()))?;
    let pdf_path = layout::book_pdf_path(&dir);
    if let Err(e) = std::fs::copy(source, &pdf_path) {
        let _ = std::fs::remove_dir_all(&dir);
        return Err(anyhow!(
            "failed to copy pdf {} -> {}: {e}",
            source.display(),
            pdf_path.display()
        ));
    }

    conn.execute(
        "INSERT INTO books (id, name, path, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![id, name, dir.display().to_string(), now_rfc3339()],
    )
    .context("insert book row failed")?;
    Ok(id)
}

/// 书库对账（books 表为唯一来源）：
/// - 确保 books 根目录存在；
/// - 为每本已有记录补齐缺失的书目录；
/// 不再扫描 books/ 下任意文件夹生成新书（手动放置的文件夹不会被发现）。
pub fn sync_from_disk(conn: &Connection) -> Result<()> {
    let root = layout::books_root();
    std::fs::create_dir_all(&root)
        .with_context(|| format!("failed to create books dir: {}", root.display()))?;
    sync_from_disk_with_root(conn, &root)
}

/// 同步入口（测试可注入根目录）
fn sync_from_disk_with_root(conn: &Connection, root: &Path) -> Result<()> {
    std::fs::create_dir_all(root)
        .with_context(|| format!("failed to create books dir: {}", root.display()))?;

    let books = list(conn)?;
    for book in &books {
        let dir = PathBuf::from(&book.path);
        if !dir.is_dir() {
            std::fs::create_dir_all(&dir)
                .with_context(|| format!("failed to create book dir: {}", dir.display()))?;
        }
    }

    Ok(())
}

/// 项目引入书：写入关联
pub fn add_to_project(conn: &Connection, project_id: &str, book_id: &str) -> Result<(), String> {
    crate::db::metadata::project::get(conn, project_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "project not found".to_string())?;
    get(conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "book not found".to_string())?;

    conn.execute(
        "INSERT OR IGNORE INTO project_books (project_id, book_id, created_at)
         VALUES (?1, ?2, ?3)",
        params![project_id, book_id, now_rfc3339()],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 项目移除书：删除关联
pub fn remove_from_project(
    conn: &Connection,
    project_id: &str,
    book_id: &str,
) -> Result<(), String> {
    crate::db::metadata::project::get(conn, project_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "project not found".to_string())?;
    get(conn, book_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "book not found".to_string())?;

    conn.execute(
        "DELETE FROM project_books WHERE project_id = ?1 AND book_id = ?2",
        params![project_id, book_id],
    )
    .map_err(|e| e.to_string())?;
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
            );
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY,
                project_id TEXT NOT NULL REFERENCES projects(id),
                parent_conversation_id TEXT,
                title TEXT DEFAULT '',
                updated_at TEXT NOT NULL,
                created_at TEXT NOT NULL,
                agent_config_id TEXT,
                status TEXT NOT NULL DEFAULT 'active'
            );
            CREATE TABLE messages (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                conversation_id TEXT NOT NULL REFERENCES conversations(id),
                role TEXT NOT NULL,
                content TEXT,
                timestamp TEXT NOT NULL,
                active TEXT NOT NULL DEFAULT 'active'
            );
            CREATE TABLE book_chats (
                book_id TEXT PRIMARY KEY REFERENCES books(id) ON DELETE CASCADE,
                conversation_id TEXT NOT NULL UNIQUE REFERENCES conversations(id) ON DELETE CASCADE,
                created_at TEXT NOT NULL
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
    fn sync_reconciles_existing_books_only() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("manual-folder")).unwrap();

        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('b1', '代数', ?1, ?2)",
            params![root.join("book-b1").display().to_string(), now_rfc3339()],
        )
        .unwrap();

        sync_from_disk_with_root(&conn, &root).unwrap();
        let books = list(&conn).unwrap();
        assert_eq!(books.len(), 1, "manual folder must not become a book");
        assert!(
            root.join("book-b1").is_dir(),
            "missing book dir should be created"
        );
        assert!(root.join("manual-folder").is_dir());

        // 幂等
        sync_from_disk_with_root(&conn, &root).unwrap();
        assert_eq!(list(&conn).unwrap().len(), 1);

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn add_remove_tracks_project_association_only() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-ref-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("library").join("book-b1")).unwrap();
        std::fs::create_dir_all(root.join("projects").join("p1")).unwrap();

        let project_path = root.join("projects").join("p1");
        insert_project(&conn, "p1", project_path.to_str().unwrap());
        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('b1', 'analysis', ?1, ?2)",
            params![
                root.join("library").join("book-b1").display().to_string(),
                now_rfc3339()
            ],
        )
        .unwrap();

        let books = list(&conn).unwrap();
        assert_eq!(books.len(), 1);
        let book_id = books[0].id.clone();

        add_to_project(&conn, "p1", &book_id).unwrap();
        let refs: i64 = conn
            .query_row(
                "SELECT count(*) FROM project_books WHERE book_id = ?1",
                params![&book_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refs, 1, "引入书应写入 project_books 关联");

        remove_from_project(&conn, "p1", &book_id).unwrap();
        let refs: i64 = conn
            .query_row(
                "SELECT count(*) FROM project_books WHERE book_id = ?1",
                params![&book_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(refs, 0, "移除书应删除 project_books 关联");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_removes_row_and_managed_dir() {
        let conn = test_conn();
        let root = std::env::temp_dir().join(format!(
            "ueberneon-books-delete-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("library").join("book-b1")).unwrap();
        std::fs::create_dir_all(root.join("projects").join("p1")).unwrap();
        std::fs::write(
            root.join("library").join("book-b1").join("original.pdf"),
            b"pdf",
        )
        .unwrap();

        let project_path = root.join("projects").join("p1");
        insert_project(&conn, "p1", project_path.to_str().unwrap());
        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('b1', 'analysis', ?1, ?2)",
            params![
                root.join("library").join("book-b1").display().to_string(),
                now_rfc3339()
            ],
        )
        .unwrap();

        add_to_project(&conn, "p1", "b1").unwrap();

        delete_at(&conn, "b1", &root.join("library")).unwrap();

        assert!(get(&conn, "b1").unwrap().is_none(), "books 行应被删除");
        assert!(
            !root.join("library").join("book-b1").exists(),
            "书库目录应被删除"
        );
        let project_count: i64 = conn
            .query_row("SELECT count(*) FROM projects WHERE id = 'p1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(project_count, 1, "删除书不应删除项目");

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn delete_keeps_external_book_dir() {
        let conn = test_conn();
        let root = std::env::temp_dir().join(format!(
            "ueberneon-books-external-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("library")).unwrap();
        std::fs::create_dir_all(root.join("external")).unwrap();
        std::fs::write(root.join("external").join("original.pdf"), b"pdf").unwrap();

        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('b1', 'external', ?1, ?2)",
            params![root.join("external").display().to_string(), now_rfc3339()],
        )
        .unwrap();

        delete_at(&conn, "b1", &root.join("library")).unwrap();

        assert!(get(&conn, "b1").unwrap().is_none());
        assert!(
            root.join("external").exists(),
            "书库根目录外的书目录不应被删除"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_legacy_links_removes_symlinks_only() {
        let conn = test_conn();
        let root = std::env::temp_dir().join(format!(
            "ueberneon-books-cleanup-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);

        let p1 = root.join("projects").join("p1");
        let p2 = root.join("projects").join("p2");
        std::fs::create_dir_all(p1.join("books")).unwrap();
        std::fs::create_dir_all(p2.join("books")).unwrap();
        std::fs::create_dir_all(root.join("library")).unwrap();
        std::os::unix::fs::symlink(
            root.join("library").join("book-b1"),
            p1.join("books").join("book-a"),
        )
        .unwrap();
        std::fs::write(p1.join("books").join("notes.txt"), b"manual").unwrap();
        std::os::unix::fs::symlink(
            root.join("library").join("book-b2"),
            p2.join("books").join("book-b"),
        )
        .unwrap();

        insert_project(&conn, "p1", p1.to_str().unwrap());
        insert_project(&conn, "p2", p2.to_str().unwrap());

        cleanup_legacy_book_links(&conn).unwrap();

        assert!(
            !p1.join("books").join("book-a").exists(),
            "旧符号链接应被删除"
        );
        assert!(
            p1.join("books").join("notes.txt").is_file(),
            "普通文件不应被删除"
        );
        assert!(p1.join("books").is_dir(), "仍含普通文件的 books 目录应保留");
        assert!(
            !p2.join("books").exists(),
            "清空后只剩空 books 目录应被移除"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn list_with_projects_groups_and_sorts() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-refs-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("library").join("book-dai")).unwrap();
        std::fs::create_dir_all(root.join("library").join("book-geo")).unwrap();
        std::fs::create_dir_all(root.join("projects").join("pa")).unwrap();
        std::fs::create_dir_all(root.join("projects").join("pb")).unwrap();

        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('dai', '代数', ?1, ?2)",
            params![
                root.join("library").join("book-dai").display().to_string(),
                now_rfc3339()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('geo', '几何', ?1, ?2)",
            params![
                root.join("library").join("book-geo").display().to_string(),
                now_rfc3339()
            ],
        )
        .unwrap();

        conn.execute(
            "INSERT INTO projects (id, name, path, created_at) VALUES ('pa', 'alpha', ?1, ?2)",
            params![
                root.join("projects").join("pa").display().to_string(),
                now_rfc3339()
            ],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO projects (id, name, path, created_at) VALUES ('pb', 'beta', ?1, ?2)",
            params![
                root.join("projects").join("pb").display().to_string(),
                now_rfc3339()
            ],
        )
        .unwrap();

        let books = list(&conn).unwrap();
        let dai = books.iter().find(|b| b.name == "代数").unwrap().id.clone();
        let geo = books.iter().find(|b| b.name == "几何").unwrap().id.clone();

        add_to_project(&conn, "pa", &dai).unwrap();
        add_to_project(&conn, "pb", &dai).unwrap();
        add_to_project(&conn, "pb", &geo).unwrap();

        let items = list_with_projects(&conn).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].book.name, "代数");
        assert_eq!(
            items[0]
                .projects
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["alpha", "beta"]
        );
        assert_eq!(items[1].book.name, "几何");
        assert_eq!(
            items[1]
                .projects
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>(),
            vec!["beta"]
        );

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

    #[test]
    fn import_pdf_file_creates_id_dir_and_copy() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-import-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("高等数学.pdf");
        std::fs::write(&src, b"fake pdf content").unwrap();

        let id = import_pdf_file_at(&conn, &src, &root).unwrap();
        let row = get(&conn, &id).unwrap().expect("book row should exist");
        assert_eq!(row.name, "高等数学");
        let dir = std::path::PathBuf::from(&row.path);
        assert_eq!(dir.file_name().unwrap().to_string_lossy(), id);
        assert_eq!(dir.join("original.pdf"), layout::book_pdf_path(&dir));
        assert_eq!(
            std::fs::read(dir.join("original.pdf")).unwrap(),
            b"fake pdf content"
        );

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn import_pdf_file_rejects_duplicate_name() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-import-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("math.pdf");
        std::fs::write(&src, b"x").unwrap();

        import_pdf_file_at(&conn, &src, &root).unwrap();
        let err = import_pdf_file_at(&conn, &src, &root).unwrap_err();
        assert!(err.to_string().contains("already exists"));

        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn import_pdf_file_missing_source_cleans_dir() {
        let conn = test_conn();
        let root =
            std::env::temp_dir().join(format!("ueberneon-books-import-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let src = root.join("missing.pdf");

        let err = import_pdf_file_at(&conn, &src, &root).unwrap_err();
        assert!(err.to_string().contains("failed to copy pdf"));
        assert!(list(&conn).unwrap().is_empty(), "no partial book row");
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != src)
            .collect();
        assert!(leftovers.is_empty(), "no leftover book dir");

        std::fs::remove_dir_all(&root).unwrap();
    }
}
