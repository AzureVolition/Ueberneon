// ── 工具 / 工具组 / 工具组关联 CRUD ──
//
// tools — 注册的工具（builtin + MCP）
// tool_groups — 工具分组
// tool_group_items — 工具与组的关联

use rusqlite::{Connection, Result, params};

// ── 行结构 ────────────────────────────────────────────────────────────────

/// tools 表行
#[derive(Debug, Clone, PartialEq)]
pub struct ToolRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub schema_json: String,
    pub read_only: bool,
    pub source: String,
    pub mcp_server: Option<String>,
    pub created_at: String,
}

/// tool_groups 表行
#[derive(Debug, Clone, PartialEq)]
pub struct ToolGroupRow {
    pub id: String,
    pub name: String,
    pub description: String,
    pub sort_order: i32,
    pub created_at: String,
}

/// tool_group_items 表行
#[derive(Debug, Clone)]
pub struct ToolGroupItemRow {
    pub group_id: String,
    pub tool_id: String,
    pub sort_order: i32,
}

/// 工具组及其包含的工具
#[derive(Debug, Clone)]
pub struct ToolGroupWithTools {
    pub group: ToolGroupRow,
    pub tools: Vec<ToolRow>,
}

// ── 工具查询 ───────────────────────────────────────────────────────────────

/// 分页查询工具列表
pub fn list_tools_paginated(
    conn: &Connection,
    group_id: Option<&str>,
    search: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<ToolRow>> {
    let mut sql = String::from(
        "SELECT t.id, t.name, t.description, t.schema_json, t.read_only, t.source, t.mcp_server, t.created_at
         FROM tools t"
    );
    let mut conditions: Vec<String> = Vec::new();

    if group_id.is_some() {
        sql.push_str(" JOIN tool_group_items tgi ON t.id = tgi.tool_id");
        conditions.push(format!("tgi.group_id = ?{}", conditions.len() + 1));
    }
    if search.is_some() {
        conditions.push(format!("t.name LIKE ?{}", conditions.len() + 1));
    }

    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(" ORDER BY t.name LIMIT ? OFFSET ?");

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        tracing::error!(target:"db", sql=%sql, error=%e, "sql error in list_tools_paginated");
        e
    })?;
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(gid) = group_id {
        params.push(Box::new(gid.to_string()));
    }
    if let Some(s) = search {
        params.push(Box::new(format!("%{}%", s)));
    }
    params.push(Box::new(limit));
    params.push(Box::new(offset));

    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    let rows = stmt.query_map(refs.as_slice(), |row| {
        Ok(ToolRow {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            schema_json: row.get(3)?,
            read_only: row.get::<_, i32>(4)? != 0,
            source: row.get(5)?,
            mcp_server: row.get(6)?,
            created_at: row.get(7)?,
        })
    })?;
    let mut result = Vec::new();
    for r in rows {
        result.push(r?);
    }
    Ok(result)
}

/// 统计工具总数（支持按组和搜索筛选）
pub fn count_tools(conn: &Connection, group_id: Option<&str>, search: Option<&str>) -> Result<i64> {
    let mut sql = String::from("SELECT count(*) FROM tools t");
    let mut conditions: Vec<String> = Vec::new();

    if group_id.is_some() {
        sql.push_str(" JOIN tool_group_items tgi ON t.id = tgi.tool_id");
        conditions.push(format!("tgi.group_id = ?{}", conditions.len() + 1));
    }
    if search.is_some() {
        conditions.push(format!("t.name LIKE ?{}", conditions.len() + 1));
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }

    let mut stmt = conn.prepare(&sql).map_err(|e| {
        tracing::error!(target:"db", sql=%sql, error=%e, "sql error in count_tools");
        e
    })?;
    let mut param_values: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    if let Some(gid) = group_id {
        param_values.push(Box::new(gid.to_string()));
    }
    if let Some(s) = search {
        param_values.push(Box::new(format!("%{}%", s)));
    }

    let params_refs: Vec<&dyn rusqlite::types::ToSql> =
        param_values.iter().map(|p| p.as_ref()).collect();
    let count: i64 = stmt.query_row(params_refs.as_slice(), |row| row.get(0))?;
    Ok(count)
}

/// 获取单个工具
pub fn get_tool(conn: &Connection, id: &str) -> Result<Option<ToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, schema_json, read_only, source, mcp_server, created_at
         FROM tools WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        Ok(ToolRow {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            schema_json: row.get(3)?,
            read_only: row.get::<_, i32>(4)? != 0,
            source: row.get(5)?,
            mcp_server: row.get(6)?,
            created_at: row.get(7)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

// ── 工具组查询 ─────────────────────────────────────────────────────────────

/// 列出所有工具组（按 sort_order 排序）
pub fn list_groups(conn: &Connection) -> Result<Vec<ToolGroupRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, sort_order, created_at
         FROM tool_groups ORDER BY sort_order, name",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ToolGroupRow {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            sort_order: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    let mut result = Vec::new();
    for r in rows {
        result.push(r?);
    }
    Ok(result)
}

/// 获取单个工具组
pub fn get_group(conn: &Connection, id: &str) -> Result<Option<ToolGroupRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, description, sort_order, created_at
         FROM tool_groups WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        Ok(ToolGroupRow {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            sort_order: row.get(3)?,
            created_at: row.get(4)?,
        })
    })?;
    match rows.next() {
        Some(r) => Ok(Some(r?)),
        None => Ok(None),
    }
}

/// 插入工具组
pub fn insert_group(conn: &Connection, row: &ToolGroupRow) -> Result<()> {
    conn.execute(
        "INSERT INTO tool_groups (id, name, description, sort_order, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            row.id,
            row.name,
            row.description,
            row.sort_order,
            row.created_at
        ],
    )?;
    Ok(())
}

/// 更新工具组
pub fn update_group(conn: &Connection, row: &ToolGroupRow) -> Result<()> {
    conn.execute(
        "UPDATE tool_groups SET name = ?1, description = ?2, sort_order = ?3 WHERE id = ?4",
        params![row.name, row.description, row.sort_order, row.id],
    )?;
    Ok(())
}

/// 删除工具组（关联项通过 ON DELETE CASCADE 自动清理）
pub fn delete_group(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM tool_groups WHERE id = ?1", params![id])?;
    Ok(())
}

// ── 工具组关联查询 ─────────────────────────────────────────────────────────

/// 获取某组内的所有工具
pub fn list_tools_in_group(conn: &Connection, group_id: &str) -> Result<Vec<ToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.description, t.schema_json, t.read_only, t.source, t.mcp_server, t.created_at
         FROM tools t
         JOIN tool_group_items tgi ON t.id = tgi.tool_id
         WHERE tgi.group_id = ?1
         ORDER BY tgi.sort_order, t.name"
    )?;
    let rows = stmt.query_map(params![group_id], |row| {
        Ok(ToolRow {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            schema_json: row.get(3)?,
            read_only: row.get::<_, i32>(4)? != 0,
            source: row.get(5)?,
            mcp_server: row.get(6)?,
            created_at: row.get(7)?,
        })
    })?;
    let mut result = Vec::new();
    for r in rows {
        result.push(r?);
    }
    Ok(result)
}

/// 获取不在某组内的所有工具
pub fn list_tools_not_in_group(conn: &Connection, group_id: &str) -> Result<Vec<ToolRow>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.name, t.description, t.schema_json, t.read_only, t.source, t.mcp_server, t.created_at
         FROM tools t
         WHERE t.id NOT IN (
             SELECT tool_id FROM tool_group_items WHERE group_id = ?1
         )
         ORDER BY t.name"
    )?;
    let rows = stmt.query_map(params![group_id], |row| {
        Ok(ToolRow {
            id: row.get(0)?,
            name: row.get(1)?,
            description: row.get(2)?,
            schema_json: row.get(3)?,
            read_only: row.get::<_, i32>(4)? != 0,
            source: row.get(5)?,
            mcp_server: row.get(6)?,
            created_at: row.get(7)?,
        })
    })?;
    let mut result = Vec::new();
    for r in rows {
        result.push(r?);
    }
    Ok(result)
}

/// 获取工具组的工具数量
pub fn count_tools_in_group(conn: &Connection, group_id: &str) -> Result<i64> {
    let count: i64 = conn.query_row(
        "SELECT count(*) FROM tool_group_items WHERE group_id = ?1",
        params![group_id],
        |row| row.get(0),
    )?;
    Ok(count)
}

/// 向组内添加一个工具
pub fn add_tool_to_group(
    conn: &Connection,
    group_id: &str,
    tool_id: &str,
    sort_order: i32,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO tool_group_items (group_id, tool_id, sort_order)
         VALUES (?1, ?2, ?3)",
        params![group_id, tool_id, sort_order],
    )?;
    Ok(())
}

/// 从组内移除一个工具
pub fn remove_tool_from_group(conn: &Connection, group_id: &str, tool_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM tool_group_items WHERE group_id = ?1 AND tool_id = ?2",
        params![group_id, tool_id],
    )?;
    Ok(())
}

// ── 集成测试 ──────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 创建内存数据库 + 建表 + 插入测试数据
    fn setup_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();

        conn.execute_batch(
            "CREATE TABLE tools (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                schema_json TEXT NOT NULL DEFAULT '{}',
                read_only   INTEGER NOT NULL DEFAULT 0,
                source      TEXT NOT NULL DEFAULT 'builtin',
                mcp_server  TEXT DEFAULT NULL,
                created_at  TEXT NOT NULL
            );
            CREATE TABLE tool_groups (
                id          TEXT PRIMARY KEY,
                name        TEXT NOT NULL UNIQUE,
                description TEXT NOT NULL DEFAULT '',
                sort_order  INTEGER NOT NULL DEFAULT 0,
                created_at  TEXT NOT NULL
            );
            CREATE TABLE tool_group_items (
                group_id    TEXT NOT NULL REFERENCES tool_groups(id) ON DELETE CASCADE,
                tool_id     TEXT NOT NULL REFERENCES tools(id) ON DELETE CASCADE,
                sort_order  INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (group_id, tool_id)
            );",
        )
        .unwrap();

        let now = chrono::Local::now().to_rfc3339();

        // 插入测试工具
        let tools = vec![
            (
                "tool-ReadFile",
                "ReadFile",
                "read file contents",
                1,
                "builtin",
            ),
            ("tool-WriteFile", "WriteFile", "write to file", 0, "builtin"),
            (
                "tool-EditFile",
                "EditFile",
                "edit file content",
                0,
                "builtin",
            ),
            ("tool-Bash", "Bash", "execute shell command", 0, "builtin"),
            ("tool-Grep", "Grep", "search with regex", 1, "builtin"),
            ("tool-Glob", "Glob", "glob pattern matching", 1, "builtin"),
            ("tool-WebFetch", "WebFetch", "fetch url", 1, "builtin"),
            (
                "tool-mcp-slide",
                "mcp__slide__create",
                "create slide deck",
                0,
                "mcp",
            ),
        ];
        for (id, name, desc, read_only, source) in &tools {
            conn.execute(
                "INSERT INTO tools (id, name, description, schema_json, read_only, source, created_at)
                 VALUES (?1, ?2, ?3, '{}', ?4, ?5, ?6)",
                params![id, name, desc, read_only, source, &now],
            ).unwrap();
        }

        // 插入测试工具组
        let groups = vec![
            ("grp-file", "File", "file operations", 1),
            ("grp-search", "Search", "search tools", 2),
            ("grp-shell", "Shell", "shell commands", 3),
            ("grp-network", "Network", "network tools", 4),
        ];
        for (id, name, desc, sort) in &groups {
            conn.execute(
                "INSERT INTO tool_groups (id, name, description, sort_order, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![id, name, desc, sort, &now],
            )
            .unwrap();
        }

        // 插入工具组关联
        let items = vec![
            ("grp-file", "tool-ReadFile", 0),
            ("grp-file", "tool-WriteFile", 1),
            ("grp-file", "tool-EditFile", 2),
            ("grp-search", "tool-Grep", 0),
            ("grp-search", "tool-Glob", 1),
            ("grp-shell", "tool-Bash", 0),
            ("grp-network", "tool-WebFetch", 0),
        ];
        for (gid, tid, sort) in &items {
            conn.execute(
                "INSERT INTO tool_group_items (group_id, tool_id, sort_order) VALUES (?1, ?2, ?3)",
                params![gid, tid, sort],
            )
            .unwrap();
        }

        conn
    }

    #[test]
    fn test_list_tools_paginated_all() {
        let conn = setup_db();
        let rows = list_tools_paginated(&conn, None, None, 10, 0).unwrap();
        assert_eq!(rows.len(), 8, "should return all 8 tools");
        assert_eq!(rows[0].name, "Bash");
    }

    #[test]
    fn test_list_tools_paginated_with_search() {
        let conn = setup_db();
        let rows = list_tools_paginated(&conn, None, Some("File"), 10, 0).unwrap();
        assert_eq!(
            rows.len(),
            3,
            "ReadFile + WriteFile + EditFile match 'File'"
        );
        for r in &rows {
            assert!(
                r.name.contains("File"),
                "name should contain 'File': {}",
                r.name
            );
        }
    }

    #[test]
    fn test_list_tools_paginated_with_group() {
        let conn = setup_db();
        let rows = list_tools_paginated(&conn, Some("grp-file"), None, 10, 0).unwrap();
        assert_eq!(rows.len(), 3, "file group has 3 tools");
        assert!(rows.iter().any(|r| r.name == "ReadFile"));
        assert!(rows.iter().any(|r| r.name == "WriteFile"));
        assert!(rows.iter().any(|r| r.name == "EditFile"));
    }

    #[test]
    fn test_list_tools_paginated_pagination() {
        let conn = setup_db();
        // 每页 3 条
        let page1 = list_tools_paginated(&conn, None, None, 3, 0).unwrap();
        assert_eq!(page1.len(), 3);
        let page2 = list_tools_paginated(&conn, None, None, 3, 3).unwrap();
        assert_eq!(page2.len(), 3);
        let page3 = list_tools_paginated(&conn, None, None, 3, 6).unwrap();
        assert_eq!(page3.len(), 2, "last page has 2 tools");
        // 页码不重叠
        for t in &page1 {
            assert!(!page2.iter().any(|x| x.id == t.id));
            assert!(!page3.iter().any(|x| x.id == t.id));
        }
    }

    #[test]
    fn test_count_tools_all() {
        let conn = setup_db();
        let count = count_tools(&conn, None, None).unwrap();
        assert_eq!(count, 8);
    }

    #[test]
    fn test_count_tools_with_search() {
        let conn = setup_db();
        let count = count_tools(&conn, None, Some("File")).unwrap();
        assert_eq!(count, 3, "ReadFile + EditFile + WriteFile match 'File'");
    }

    #[test]
    fn test_count_tools_with_group() {
        let conn = setup_db();
        let count = count_tools(&conn, Some("grp-shell"), None).unwrap();
        assert_eq!(count, 1, "shell group has 1 tool");
    }

    #[test]
    fn test_list_groups() {
        let conn = setup_db();
        let groups = list_groups(&conn).unwrap();
        assert_eq!(groups.len(), 4);
        assert_eq!(groups[0].name, "File");
        assert_eq!(groups[3].name, "Network");
    }

    #[test]
    fn test_insert_and_delete_group() {
        let conn = setup_db();
        let now = chrono::Local::now().to_rfc3339();
        let g = ToolGroupRow {
            id: "grp-test".into(),
            name: "Test".into(),
            description: "test group".into(),
            sort_order: 5,
            created_at: now,
        };
        insert_group(&conn, &g).unwrap();
        assert_eq!(list_groups(&conn).unwrap().len(), 5);

        delete_group(&conn, "grp-test").unwrap();
        assert_eq!(list_groups(&conn).unwrap().len(), 4);
    }

    #[test]
    fn test_tools_in_group() {
        let conn = setup_db();
        let tools = list_tools_in_group(&conn, "grp-file").unwrap();
        assert_eq!(tools.len(), 3);
        assert_eq!(tools[0].name, "ReadFile");
    }

    #[test]
    fn test_add_and_remove_tool_from_group() {
        let conn = setup_db();
        // Bash 不在 file 组
        let before = list_tools_in_group(&conn, "grp-file").unwrap();
        assert_eq!(before.len(), 3);
        assert!(!before.iter().any(|t| t.name == "Bash"));

        // 添加
        add_tool_to_group(&conn, "grp-file", "tool-Bash", 5).unwrap();
        let after_add = list_tools_in_group(&conn, "grp-file").unwrap();
        assert_eq!(after_add.len(), 4);
        assert!(after_add.iter().any(|t| t.name == "Bash"));

        // 移除
        remove_tool_from_group(&conn, "grp-file", "tool-Bash").unwrap();
        let after_remove = list_tools_in_group(&conn, "grp-file").unwrap();
        assert_eq!(after_remove.len(), 3);
    }

    #[test]
    fn test_count_tools_in_group() {
        let conn = setup_db();
        assert_eq!(count_tools_in_group(&conn, "grp-search").unwrap(), 2);
        assert_eq!(count_tools_in_group(&conn, "grp-network").unwrap(), 1);
        assert_eq!(count_tools_in_group(&conn, "grp-nonexistent").unwrap(), 0);
    }

    #[test]
    fn test_get_tool() {
        let conn = setup_db();
        let t = get_tool(&conn, "tool-Bash")
            .unwrap()
            .expect("Bash should exist");
        assert_eq!(t.name, "Bash");
        assert!(!t.read_only, "Bash is not read-only");
    }

    #[test]
    fn test_update_group() {
        let conn = setup_db();
        let now = chrono::Local::now().to_rfc3339();
        let g = ToolGroupRow {
            id: "grp-test-upd".into(),
            name: "UpdateMe".into(),
            description: "before".into(),
            sort_order: 9,
            created_at: now.clone(),
        };
        insert_group(&conn, &g).unwrap();

        let updated = ToolGroupRow {
            id: "grp-test-upd".into(),
            name: "Updated".into(),
            description: "after".into(),
            sort_order: 10,
            created_at: now,
        };
        update_group(&conn, &updated).unwrap();

        let fetched = get_group(&conn, "grp-test-upd").unwrap().unwrap();
        assert_eq!(fetched.name, "Updated");
        assert_eq!(fetched.description, "after");
        assert_eq!(fetched.sort_order, 10);
    }

    #[test]
    fn test_list_tools_not_in_group() {
        let conn = setup_db();
        let not_in_file = list_tools_not_in_group(&conn, "grp-file").unwrap();
        // total 8 tools, 3 in file → 5 not in file
        assert_eq!(not_in_file.len(), 5);
        assert!(!not_in_file.iter().any(|t| t.name == "ReadFile"));
        assert!(not_in_file.iter().any(|t| t.name == "Bash"));
    }
}
