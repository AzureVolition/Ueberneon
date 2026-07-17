// ── 数据库层 ──
//
// 使用 SQLite (rusqlite) 管理 projects / conversations / messages。
// 数据库文件存储在 ~/.racpagent/data.db
//
// 当前阶段：仅引入 + 建表，store.rs 仍使用 JSON 文件。
// 后续可将 store.rs 替换为基于该模块的 CRUD 实现。

pub mod metadata;

use std::sync::{Mutex, OnceLock};

use rusqlite::{Connection, Result};

/// 默认项目的固定 id（与 store.rs 迁移至此）
pub const DEFAULT_PROJECT_ID: &str = "racpagent-default";

/// 数据库文件路径：~/.racpagent/data.db
fn db_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    std::path::PathBuf::from(home).join(".racpagent").join("data.db")
}

/// 初始化数据库。
///
/// - 确保 ~/.racpagent/ 目录存在
/// - 打开/创建 data.db
/// - 启用 WAL 模式
/// - 执行 CREATE TABLE IF NOT EXISTS + CREATE INDEX IF NOT EXISTS
///
/// 返回打开后的 Connection，调用方应持有该连接以供后续读写。
pub fn init_db() -> Result<Connection> {
    let path = db_path();

    // 确保目录存在
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let conn = Connection::open(&path)?;

    // WAL 模式：写入性能好，且允许并发读取
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;

    // 外键约束
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;

    // ── 建表 ──────────────────────────────────────────────────────────────

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS projects (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            path            TEXT NOT NULL,
            created_at      TEXT NOT NULL,
            indicator_color TEXT DEFAULT '',
            last_activity_at TEXT
        );

        CREATE TABLE IF NOT EXISTS conversations (
            id          TEXT PRIMARY KEY,
            project_id  TEXT NOT NULL REFERENCES projects(id),
            title       TEXT DEFAULT '',
            updated_at  TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS messages (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id     TEXT NOT NULL REFERENCES conversations(id),
            role                TEXT NOT NULL,
            content             TEXT,
            timestamp           TEXT NOT NULL,
            reasoning_content   TEXT,
            reasoning_signature TEXT,
            tool_calls          TEXT,
            tool_call_id        TEXT,
            name                TEXT,
            images              TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_conversations_project
            ON conversations(project_id);

        CREATE INDEX IF NOT EXISTS idx_messages_conversation
            ON messages(conversation_id, timestamp);"
    )?;

    // ── 默认项目 ──────────────────────────────────────────────────────────
    // 确保 "racpagent" 默认项目始终存在
    conn.execute(
        "INSERT OR IGNORE INTO projects (id, name, path, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            DEFAULT_PROJECT_ID,
            "racpagent",
            db_path().parent().unwrap().to_string_lossy().to_string(),
            chrono::Local::now().to_rfc3339(),
        ],
    )?;

    Ok(conn)
}

/// 全局 DB 连接（懒加载）。整个应用生命周期内只需初始化一次。
static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

/// 获取全局 DB 连接。首次调用时自动执行 init_db() 初始化。
pub fn get_db() -> &'static Mutex<Connection> {
    DB.get_or_init(|| {
        Mutex::new(init_db().expect("failed to initialize database"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_init_db_creates_file() {
        // 使用临时目录测试
        let tmp = std::env::temp_dir().join(format!("racpagent-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();

        // 临时覆盖 HOME 环境变量
        let original_home = std::env::var("HOME").ok();
        unsafe { std::env::set_var("HOME", tmp.to_str().unwrap()); }

        let conn = init_db().expect("init_db should succeed");
        let db_file = tmp.join(".racpagent").join("data.db");
        assert!(db_file.exists(), "database file should exist");

        // 验证表存在
        let tables: Vec<String> = conn
            .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert!(tables.contains(&"projects".to_string()), "projects table");
        assert!(tables.contains(&"conversations".to_string()), "conversations table");
        assert!(tables.contains(&"messages".to_string()), "messages table");

        // 验证索引存在
        let idx_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(idx_count, 2, "should have 2 indexes");

        // 验证默认项目已插入
        let default_count: i64 = conn
            .query_row(
                "SELECT count(*) FROM projects WHERE id = ?1",
                [DEFAULT_PROJECT_ID],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(default_count, 1, "default project should exist");

        // 清理
        drop(conn);
        std::fs::remove_dir_all(&tmp).unwrap();
        if let Some(home) = original_home {
            unsafe { std::env::set_var("HOME", home); }
        }
    }
}
