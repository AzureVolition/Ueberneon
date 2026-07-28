// ── 数据库层 ──
//
// 使用 SQLite (rusqlite) 管理 projects / conversations / messages。
// 数据库文件存储在 ~/.racpagent/data.db
//
// 当前阶段：仅引入 + 建表，store.rs 仍使用 JSON 文件。
// 后续可将 store.rs 替换为基于该模块的 CRUD 实现。

pub mod metadata;
pub mod model_fetch;
pub mod provider_presets;

use std::sync::{Mutex, OnceLock};

use anyhow::Context;
use rusqlite::Connection;

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
pub fn init_db() -> anyhow::Result<Connection> {
    let path = db_path();

    // 确保目录存在
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let mut conn = Connection::open(&path)?;

    // ── SQL 日志 ──
    conn.trace(Some(|sql| {
        tracing::debug!("[SQL] {sql}");
    }));

    // WAL 模式
    conn.execute_batch("PRAGMA journal_mode=WAL;")?;

    // 外键约束
    conn.execute_batch("PRAGMA foreign_keys=ON;")?;

    // 建表 + 种子数据
    rebuild_schema(&conn)?;

    Ok(conn)
}

/// 建表 + 种子数据（接收 &Connection，不持有所有权）
fn rebuild_schema(conn: &Connection) -> anyhow::Result<()> {

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
            parent_conversation_id TEXT REFERENCES conversations(id),
            title       TEXT DEFAULT '',
            updated_at  TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            agent_config_id TEXT REFERENCES agent_configs(id),
            status      TEXT NOT NULL DEFAULT 'active',
            total_prompt_tokens    INTEGER NOT NULL DEFAULT 0,
            total_completion_tokens INTEGER NOT NULL DEFAULT 0,
            total_reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
            total_tokens            INTEGER NOT NULL DEFAULT 0,
            request_count           INTEGER NOT NULL DEFAULT 0,
            cache_hit_tokens        INTEGER NOT NULL DEFAULT 0,
            cache_miss_tokens       INTEGER NOT NULL DEFAULT 0,
            last_usage_at           TEXT
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
            tool_name           TEXT,
            images              TEXT,
            active              TEXT NOT NULL DEFAULT 'active'
        );

        CREATE INDEX IF NOT EXISTS idx_conversations_project
            ON conversations(project_id);

        CREATE INDEX IF NOT EXISTS idx_messages_conv_active_ts
            ON messages(conversation_id, active, timestamp);

        CREATE INDEX IF NOT EXISTS idx_messages_conversation
            ON messages(conversation_id, timestamp);

        CREATE TABLE IF NOT EXISTS providers (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            kind            TEXT NOT NULL DEFAULT 'openai',
            base_url        TEXT NOT NULL,
            models_url      TEXT DEFAULT '',
            balance_url     TEXT DEFAULT '',
            context_window  INTEGER,
            is_preset       INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS provider_models (
            provider_id     TEXT NOT NULL REFERENCES providers(id) ON DELETE CASCADE,
            model_name      TEXT NOT NULL,
            PRIMARY KEY (provider_id, model_name)
        );

        CREATE TABLE IF NOT EXISTS provider_instances (
            id              TEXT PRIMARY KEY,
            provider_id     TEXT NOT NULL REFERENCES providers(id),
            alias           TEXT NOT NULL,
            api_key         TEXT NOT NULL DEFAULT '',
            sort_order      INTEGER NOT NULL DEFAULT 0,
            created_at      TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS agent_configs (
            id                  TEXT PRIMARY KEY,
            name                TEXT NOT NULL UNIQUE,
            agent_type          TEXT NOT NULL DEFAULT 'Custom',
            provider_instance_id TEXT NOT NULL DEFAULT '',
            model               TEXT NOT NULL DEFAULT '',
            base_url            TEXT NOT NULL DEFAULT '',
            api_key             TEXT NOT NULL DEFAULT '',
            system_prompt       TEXT NOT NULL DEFAULT '',
            temperature         REAL NOT NULL DEFAULT 0.7,
            max_tokens          INTEGER,
            context_window      INTEGER,
            tools               TEXT NOT NULL DEFAULT '[]',
            description         TEXT NOT NULL DEFAULT '',
            created_at          TEXT NOT NULL,
            updated_at          TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_configs_name ON agent_configs(name);

        -- ── 工具表（builtin + MCP）──
        CREATE TABLE IF NOT EXISTS tools (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            description TEXT NOT NULL DEFAULT '',
            schema_json TEXT NOT NULL DEFAULT '{}',
            read_only   INTEGER NOT NULL DEFAULT 0,
            source      TEXT NOT NULL DEFAULT 'builtin',
            mcp_server  TEXT DEFAULT NULL,
            created_at  TEXT NOT NULL
        );

        -- ── 工具组 ──
        CREATE TABLE IF NOT EXISTS tool_groups (
            id          TEXT PRIMARY KEY,
            name        TEXT NOT NULL UNIQUE,
            description TEXT NOT NULL DEFAULT '',
            sort_order  INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL
        );

        -- ── 工具组-工具关联 ──
        CREATE TABLE IF NOT EXISTS tool_group_items (
            group_id    TEXT NOT NULL REFERENCES tool_groups(id) ON DELETE CASCADE,
            tool_id     TEXT NOT NULL REFERENCES tools(id) ON DELETE CASCADE,
            sort_order  INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (group_id, tool_id)
        );

        -- ── Agent 配置-工具组关联 ──
        CREATE TABLE IF NOT EXISTS agent_config_groups (
            agent_config_id TEXT NOT NULL REFERENCES agent_configs(id) ON DELETE CASCADE,
            tool_group_id   TEXT NOT NULL REFERENCES tool_groups(id) ON DELETE CASCADE,
            PRIMARY KEY (agent_config_id, tool_group_id)
        );

        -- ── 计划 ──
        CREATE TABLE IF NOT EXISTS plans (
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

        CREATE INDEX IF NOT EXISTS idx_plans_project
            ON plans(project_id, created_at);

        CREATE INDEX IF NOT EXISTS idx_plans_conversation
            ON plans(conversation_id);

        -- ── 任务（支持父子关系，最多两层） ──
        CREATE TABLE IF NOT EXISTS tasks (
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

        CREATE INDEX IF NOT EXISTS idx_tasks_plan
            ON tasks(plan_id, idx);

        CREATE INDEX IF NOT EXISTS idx_tasks_project
            ON tasks(project_id);

        CREATE INDEX IF NOT EXISTS idx_tasks_parent
            ON tasks(parent_task_id);
        "
    )?;

    // migration: add status column for existing databases (ignore error if exists)
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN status TEXT NOT NULL DEFAULT 'active'", []);

    // migration: add description column for existing databases (ignore error if exists)
    let _ = conn.execute("ALTER TABLE agent_configs ADD COLUMN description TEXT NOT NULL DEFAULT ''", []);

    // migration: token usage tracking columns for conversations
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN total_prompt_tokens INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN total_completion_tokens INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN total_reasoning_tokens INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN total_tokens INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN request_count INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN cache_hit_tokens INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN cache_miss_tokens INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE conversations ADD COLUMN last_usage_at TEXT", []);

    // migration: context_window for agent_configs
    let _ = conn.execute("ALTER TABLE agent_configs ADD COLUMN context_window INTEGER", []);

    // ── 默认项目 ──────────────────────────────────────────────────────────
    // 确保 "racpagent" 默认项目始终存在
    conn.execute(
        "INSERT OR IGNORE INTO projects (id, name, path, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            DEFAULT_PROJECT_ID,
            "racpagent",
            db_path().parent().context("db_path has no parent directory")?.to_string_lossy().to_string(),
            chrono::Local::now().to_rfc3339(),
        ],
    )?;

    // ── Provider 预设 ─────────────────────────────────────────────────────
    // 幂等插入所有内置 provider 预设
    for preset in provider_presets::all_presets() {
        conn.execute(
            "INSERT OR IGNORE INTO providers (id, name, kind, base_url, models_url, context_window, is_preset)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1)",
            rusqlite::params![preset.id, preset.name, preset.kind, preset.base_url, preset.models_url, preset.context_window],
        )?;
        // 插入预设模型列表
        if !preset.models.is_empty() {
            let mut stmt = conn.prepare(
                "INSERT OR IGNORE INTO provider_models (provider_id, model_name) VALUES (?1, ?2)"
            )?;
            for model in preset.models {
                stmt.execute(rusqlite::params![preset.id, model])?;
            }
        }
    }

    // ── 同步内置工具到 tools 表 ──────────────────────────────────────────
    sync_builtin_tools(&conn)?;

    // ── 默认工具组（幂等插入）──
    seed_default_tool_groups(&conn)?;

    // ── 内置 Explore SubAgent ─────────────────────────────────────────────
    
    // 幂等插入 explore 子 agent（只读文件搜索专家）
    // provider 信息由用户在 Sub Agents 页面中配置
    conn.execute(
        "INSERT OR REPLACE INTO agent_configs (id, name, agent_type, system_prompt, temperature, max_tokens, tools, description, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        rusqlite::params![
            "acfg-builtin-explore",
            "explore",
            "SubAgent",
            crate::agent::prompts::explore::EXPLORE_SUBAGENT_PROMPT,
            0.7,
            Option::<u32>::None,
            r#"["ReadFile","Grep","Glob","Ls","CodeIndex","WebFetch","ReadOnlyBash"]"#,
            "只读代码搜索专家，快速探索代码库并输出搜索报告",
            chrono::Local::now().to_rfc3339(),
            chrono::Local::now().to_rfc3339(),
        ],
    )?;

    

    Ok(())
}

/// 全局 DB 连接（懒加载）。整个应用生命周期内只需初始化一次。
static DB: OnceLock<Mutex<Connection>> = OnceLock::new();

/// 获取全局 DB 连接。首次调用时自动执行 init_db() 初始化。
pub fn get_db() -> &'static Mutex<Connection> {
    DB.get_or_init(|| {
        Mutex::new(init_db().expect("failed to initialize database"))
    })
}

/// 在闭包内使用 DB 连接，闭包结束后自动释放锁。
/// 调用方无需手动获取/释放 MutexGuard。
///
/// # Panics
/// 在锁被 poison 时 panic（与直接调用 `lock().unwrap()` 行为一致）。
pub fn with_db<T>(f: impl FnOnce(&Connection) -> T) -> T {
    let guard = get_db().lock().expect("db lock poisoned");
    f(&guard)
}

/// 与 [`with_db`] 相同，但返回 `Result` —— 锁被 poison 时返回 `Err(...)`。
pub fn with_db_result<T, E>(
    f: impl FnOnce(&Connection) -> Result<T, E>,
) -> Result<T, String>
where
    E: std::fmt::Display,
{
    let guard = get_db()
        .lock()
        .map_err(|e| format!("failed to acquire db lock: {e}"))?;
    f(&guard).map_err(|e| format!("db error: {e}"))
}

/// 容错版：锁获取失败（poison）或未初始化时静默跳过。
/// 适用于日志记录等非关键路径。
pub fn try_with_db(f: impl FnOnce(&Connection)) {
    if let Ok(guard) = get_db().lock() {
        f(&guard);
    }
}

/// 将 inventory 中的内置工具同步到 tools 表（幂等）。
fn sync_builtin_tools(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    let now = chrono::Local::now().to_rfc3339();
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO tools (id, name, description, schema_json, read_only, source, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'builtin', ?6)",
    )?;
    for meta in inventory::iter::<crate::tools::InternalToolMeta>().into_iter() {
        let id = format!("tool-{}", meta.name);
        stmt.execute(rusqlite::params![
            id,
            meta.name,
            meta.description,
            if meta.schema.is_empty() { "{}" } else { meta.schema },
            meta.read_only as i32,
            &now,
        ])?;
    }
    Ok(())
}

/// 插入默认工具组（幂等，仅在 tools 表有数据时执行）。
fn seed_default_tool_groups(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    // 仅在工具表非空时才执行（测试环境中 inventory 可能为空）
    let count: i64 = conn.query_row("SELECT count(*) FROM tools", [], |r| r.get(0))?;
    if count == 0 {
        return Ok(());
    }
    let now = chrono::Local::now().to_rfc3339();
    // 文件操作组
    conn.execute(
        "INSERT OR IGNORE INTO tool_groups (id, name, description, sort_order, created_at)
         VALUES ('grp-file', 'File', '读写和编辑文件', 1, ?1)",
        rusqlite::params![&now],
    )?;
    // 搜索组
    conn.execute(
        "INSERT OR IGNORE INTO tool_groups (id, name, description, sort_order, created_at)
         VALUES ('grp-search', 'Search', '搜索文件和代码', 2, ?1)",
        rusqlite::params![&now],
    )?;
    // Shell 组
    conn.execute(
        "INSERT OR IGNORE INTO tool_groups (id, name, description, sort_order, created_at)
         VALUES ('grp-shell', 'Shell', '执行 shell 命令', 3, ?1)",
        rusqlite::params![&now],
    )?;
    // 网络组
    conn.execute(
        "INSERT OR IGNORE INTO tool_groups (id, name, description, sort_order, created_at)
         VALUES ('grp-network', 'Network', '网络请求', 4, ?1)",
        rusqlite::params![&now],
    )?;
    // 工具到组的关联（幂等）
    let groups: &[(&str, &[&str])] = &[
        ("grp-file",    &["ReadFile", "WriteFile", "EditFile", "MultiEdit", "Ls"]),
        ("grp-search",  &["Grep", "Glob", "CodeIndex"]),
        ("grp-shell",   &["Bash", "BashOutput", "KillShell", "ReadOnlyBash"]),
        ("grp-network", &["WebFetch"]),
    ];
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO tool_group_items (group_id, tool_id, sort_order)
         VALUES (?1, ?2, ?3)",
    )?;
    for (group_id, tool_names) in groups {
        for (i, name) in tool_names.iter().enumerate() {
            let tool_id = format!("tool-{}", name);
            stmt.execute(rusqlite::params![group_id, tool_id, i as i32])?;
        }
    }
    Ok(())
}

/// 删除所有表并重新初始化（开发用）
pub fn drop_all_tables() -> anyhow::Result<() > {
    with_db(|conn| -> anyhow::Result<()> {
        conn.execute_batch("PRAGMA foreign_keys = OFF;")?;
        let tables: Vec<String> = {
            let mut stmt = conn.prepare(
                "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name"
            )?;
            stmt.query_map([], |row| row.get(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        for t in &tables {
            conn.execute(&format!("DROP TABLE IF EXISTS \"{}\"", t), [])?;
        }
        rebuild_schema(conn)?;
        Ok(())
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
