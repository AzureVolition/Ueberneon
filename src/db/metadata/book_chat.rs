// ── BookChat CRUD ──
//
// 每本书一个全局对话：book_id → conversation_id 映射。
// conversation 使用 status='sub_agent'，不进入普通对话列表。

use rusqlite::{Connection, Result, params};

/// book_chats 表行
#[derive(Debug, Clone)]
pub struct BookChatRow {
    pub book_id: String,
    pub conversation_id: String,
    pub created_at: String,
}

/// 按书 ID 查询
pub fn get_by_book(conn: &Connection, book_id: &str) -> Result<Option<BookChatRow>> {
    let mut stmt = conn.prepare(
        "SELECT book_id, conversation_id, created_at FROM book_chats WHERE book_id = ?1",
    )?;
    let mut rows = stmt.query_map(params![book_id], |row| {
        Ok(BookChatRow {
            book_id: row.get(0)?,
            conversation_id: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 按对话 ID 查询（用于构建 Agent 时识别书聊并启用压缩）
pub fn get_by_conversation(conn: &Connection, conversation_id: &str) -> Result<Option<BookChatRow>> {
    let mut stmt = conn.prepare(
        "SELECT book_id, conversation_id, created_at FROM book_chats WHERE conversation_id = ?1",
    )?;
    let mut rows = stmt.query_map(params![conversation_id], |row| {
        Ok(BookChatRow {
            book_id: row.get(0)?,
            conversation_id: row.get(1)?,
            created_at: row.get(2)?,
        })
    })?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 插入映射（每本书唯一）
pub fn create(conn: &Connection, book_id: &str, conversation_id: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO book_chats (book_id, conversation_id, created_at)
         VALUES (?1, ?2, ?3)",
        params![book_id, conversation_id, chrono::Local::now().to_rfc3339()],
    )?;
    Ok(())
}

/// 按书删除映射
pub fn delete_by_book(conn: &Connection, book_id: &str) -> Result<()> {
    conn.execute("DELETE FROM book_chats WHERE book_id = ?1", params![book_id])?;
    Ok(())
}

/// 按对话删除映射
pub fn delete_by_conversation(conn: &Connection, conversation_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM book_chats WHERE conversation_id = ?1",
        params![conversation_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE books (
                id TEXT PRIMARY KEY, name TEXT NOT NULL UNIQUE,
                path TEXT NOT NULL, created_at TEXT NOT NULL
            );
            CREATE TABLE conversations (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL,
                parent_conversation_id TEXT, title TEXT DEFAULT '',
                updated_at TEXT NOT NULL, created_at TEXT NOT NULL,
                agent_config_id TEXT, status TEXT NOT NULL DEFAULT 'active'
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

    #[test]
    fn create_and_lookup_by_book_and_conversation() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('b1', 'book', '/b', 't')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations (id, project_id, updated_at, created_at, status)
             VALUES ('c1', 'p1', 't', 't', 'sub_agent')",
            [],
        )
        .unwrap();
        create(&conn, "b1", "c1").unwrap();

        let by_book = get_by_book(&conn, "b1").unwrap().unwrap();
        assert_eq!(by_book.conversation_id, "c1");
        let by_conv = get_by_conversation(&conn, "c1").unwrap().unwrap();
        assert_eq!(by_conv.book_id, "b1");
    }

    #[test]
    fn delete_by_book_removes_mapping() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO books (id, name, path, created_at) VALUES ('b1', 'book', '/b', 't')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO conversations (id, project_id, updated_at, created_at, status)
             VALUES ('c1', 'p1', 't', 't', 'sub_agent')",
            [],
        )
        .unwrap();
        create(&conn, "b1", "c1").unwrap();
        delete_by_book(&conn, "b1").unwrap();
        assert!(get_by_book(&conn, "b1").unwrap().is_none());
    }
}
