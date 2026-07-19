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

use rusqlite::{Connection, Result};

/// 默认项目的固定 id（与 store.rs 迁移至此）
pub const DEFAULT_PROJECT_ID: &str = "racpagent-default";

/// Explore SubAgent 系统提示词（来自 Claude Code Explore Agent v2.1.7）
const EXPLORE_SUBAGENT_PROMPT: &str = r#"You are a file search specialist for Claude Code, Anthropic's official CLI for Claude. You excel at thoroughly navigating and exploring codebases.

---

### === CRITICAL: READ-ONLY MODE - NO FILE MODIFICATIONS ===

This is a **READ-ONLY** exploration task. You are **STRICTLY PROHIBITED** from:
- Creating new files (no Write, touch, or file creation of any kind)
- Modifying existing files (no Edit operations)
- Deleting files (no rm or deletion)
- Moving or copying files (no mv or cp)
- Creating temporary files anywhere, including /tmp
- Using redirect operators (>, >>, |) or heredocs to write to files
- Running ANY commands that change system state

Your role is **EXCLUSIVELY** to search and analyze existing code. You do NOT have access to file editing tools - attempting to edit files will fail.

---

### Your Strengths
- Rapidly finding files using glob patterns
- Searching code and text with powerful regex patterns
- Reading and analyzing file contents

### Guidelines
- Use **Glob** for broad file pattern matching
- Use **Grep** for searching file contents with regex
- Use **FileRead** when you know the specific file path you need to read
- Use **RunCommand** ONLY for read-only operations (ls, git status, git log, git diff, find, cat, head, tail)
- **NEVER** use **RunCommand** for: mkdir, touch, rm, cp, mv, git add, git commit, npm install, pip install, or any file creation/modification
- Adapt your search approach based on the thoroughness level specified by the caller
- Return file paths as absolute paths in your final response
- For clear communication, avoid using emojis
- Communicate your final report directly as a regular message - do NOT attempt to create files

### Efficiency Requirements
NOTE: You are meant to be a fast agent that returns output as quickly as possible. In order to achieve this you must:
- Make efficient use of the tools that you have at your disposal: be smart about how you search for files and implementations
- Wherever possible you should try to spawn multiple parallel tool calls for grepping and reading files

Complete the user's search request efficiently and report your findings clearly."#;

/// Plan SubAgent 系统提示词（来自 Claude Code Plan Mode v2.1.7）
const PLAN_SUBAGENT_PROMPT: &str = r#"Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supersedes any other instructions you have received.

You should build your plan incrementally by writing to or editing a plan file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.

---

## 5-Phase Plan Workflow

### Phase 1: Initial Understanding
Goal: Gain a comprehensive understanding of the user's request by reading through code and asking them questions.
1. Focus on understanding the user's request and the code associated with their request.
2. Launch Explore agents to efficiently explore the codebase.
3. After exploring the code, ask clarifying questions to resolve ambiguities.

### Phase 2: Design
Goal: Design an implementation approach based on the user's intent and your exploration results.
- Consider alternatives and validate your understanding.
- Produce a detailed implementation plan with file paths and code traces.

### Phase 3: Review
Goal: Review the plan and ensure alignment with the user's intentions.
1. Read critical files to deepen understanding.
2. Ensure plans align with the user's original request.

### Phase 4: Final Plan
Goal: Write the final plan.
- Include only the recommended approach, not all alternatives.
- Keep it concise but detailed enough to execute.
- Include paths of critical files to be modified.
- Include a verification section describing how to test changes end-to-end.

### Phase 5: Request Approval
Call ExitPlanMode to indicate planning is complete and request user approval.
Only stop for clarification questions or plan approval - do not execute without approval."#;

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

    let mut conn = Connection::open(&path)?;

    // ── SQL 日志 ──
    conn.trace(Some(|sql| {
        tracing::debug!("[SQL] {sql}");
    }));

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
            updated_at  TEXT NOT NULL,
            agent_config_id TEXT REFERENCES agent_configs(id)
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

        CREATE INDEX IF NOT EXISTS idx_messages_conversation
            ON messages(conversation_id, timestamp);

        CREATE TABLE IF NOT EXISTS providers (
            id              TEXT PRIMARY KEY,
            name            TEXT NOT NULL,
            kind            TEXT NOT NULL DEFAULT 'openai',
            base_url        TEXT NOT NULL,
            models_url      TEXT DEFAULT '',
            balance_url     TEXT DEFAULT '',
            context_window  INTEGER NOT NULL DEFAULT 131072,
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
            tools               TEXT NOT NULL DEFAULT '[]',
            created_at          TEXT NOT NULL,
            updated_at          TEXT NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_agent_configs_name ON agent_configs(name);
        "
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

    // ── 内置 Explore SubAgent ─────────────────────────────────────────────
    // 幂等插入 explore 子 agent（只读文件搜索专家）
    // provider 信息由用户在 Sub Agents 页面中配置
    conn.execute(
        "INSERT OR IGNORE INTO agent_configs (id, name, agent_type, system_prompt, temperature, max_tokens, tools, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            "acfg-builtin-explore",
            "explore",
            "SubAgent",
            EXPLORE_SUBAGENT_PROMPT,
            0.7,
            Option::<u32>::None,
            r#"["read_file","grep","glob","ls","code_index","web_fetch","read_only_bash"]"#,
            chrono::Local::now().to_rfc3339(),
            chrono::Local::now().to_rfc3339(),
        ],
    )?;

    // ── 内置 Plan SubAgent ────────────────────────────────────────────────
    conn.execute(
        "INSERT OR IGNORE INTO agent_configs (id, name, agent_type, system_prompt, temperature, max_tokens, tools, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        rusqlite::params![
            "acfg-builtin-plan",
            "plan",
            "SubAgent",
            PLAN_SUBAGENT_PROMPT,
            0.7,
            Option::<u32>::None,
            r#"["read_file","grep","glob","ls","code_index","web_fetch","read_only_bash"]"#,
            chrono::Local::now().to_rfc3339(),
            chrono::Local::now().to_rfc3339(),
        ],
    )?;

    // ── 迁移：将旧版 config.json 中的 provider_keys 转为实例 ────────────
    // 为每个已有 key 的 provider 创建一条实例记录（幂等）
    let config_path = std::path::PathBuf::from(
        std::env::var("HOME").unwrap_or_else(|_| "/tmp".into())
    ).join(".racpagent").join("config.json");
    if config_path.exists() {
        if let Ok(json) = std::fs::read_to_string(&config_path) {
            if let Ok(old_config) = serde_json::from_str::<serde_json::Value>(&json) {
                if let Some(keys) = old_config.get("provider_keys").and_then(|v| v.as_object()) {
                    let now = chrono::Local::now().to_rfc3339();
                    for (prov_id, key_val) in keys {
                        // 检查该 provider 是否存在
                        let exists: bool = conn.query_row(
                            "SELECT 1 FROM providers WHERE id = ?1",
                            rusqlite::params![prov_id],
                            |_| Ok(()),
                        ).is_ok();
                        if !exists { continue; }
                        // 检查是否已创建过实例
                        let already: bool = conn.query_row(
                            "SELECT 1 FROM provider_instances WHERE provider_id = ?1",
                            rusqlite::params![prov_id],
                            |_| Ok(()),
                        ).is_ok();
                        if already { continue; }
                        // 生成唯一 ID（时间戳 + 进程 ID）
                        use std::time::{SystemTime, UNIX_EPOCH};
                        let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
                        let pid = std::process::id();
                        let instance_id = format!("inst-{ts:x}-{pid:x}");
                        let alias = prov_id.clone();
                        let key_str = key_val.as_str().unwrap_or("");
                        conn.execute(
                            "INSERT INTO provider_instances (id, provider_id, alias, api_key, sort_order, created_at)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            rusqlite::params![instance_id, prov_id, alias, key_str, 0, now],
                        )?;
                    }
                }
            }
        }
    }

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
