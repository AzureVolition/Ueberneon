// ── Provider 实例 CRUD ──
//
// provider_instance = 用户实际接入的一个 LLM 服务。
// 每个实例引用 providers 表中的一条预设（provider_id），
// 并保存用户自己的 alias 和 api_key。

use rusqlite::{Connection, Result, params};

/// 数据库行
#[derive(Debug, Clone)]
pub struct ProviderInstanceRow {
    pub id: String,
    pub provider_id: String,
    pub alias: String,
    pub api_key: String,
    pub sort_order: i32,
    pub created_at: String,
}

/// 插入一条实例
pub fn insert(conn: &Connection, row: &ProviderInstanceRow) -> Result<()> {
    conn.execute(
        "INSERT INTO provider_instances (id, provider_id, alias, api_key, sort_order, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            row.id,
            row.provider_id,
            row.alias,
            row.api_key,
            row.sort_order,
            row.created_at
        ],
    )?;
    Ok(())
}

/// 删除实例
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM provider_instances WHERE id = ?1", params![id])?;
    Ok(())
}

/// 获取单个实例
pub fn get(conn: &Connection, id: &str) -> Result<Option<ProviderInstanceRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider_id, alias, api_key, sort_order, created_at
         FROM provider_instances WHERE id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        Ok(ProviderInstanceRow {
            id: row.get(0)?,
            provider_id: row.get(1)?,
            alias: row.get(2)?,
            api_key: row.get(3)?,
            sort_order: row.get::<_, i32>(4)?,
            created_at: row.get(5)?,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        _ => Ok(None),
    }
}

/// 列出所有实例（按 sort_order 排序）
pub fn list_all(conn: &Connection) -> Result<Vec<ProviderInstanceRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, provider_id, alias, api_key, sort_order, created_at
         FROM provider_instances ORDER BY sort_order ASC, created_at ASC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ProviderInstanceRow {
            id: row.get(0)?,
            provider_id: row.get(1)?,
            alias: row.get(2)?,
            api_key: row.get(3)?,
            sort_order: row.get::<_, i32>(4)?,
            created_at: row.get(5)?,
        })
    })?;
    rows.collect()
}

/// 更新实例的 api_key
pub fn update_key(conn: &Connection, id: &str, api_key: &str) -> Result<()> {
    conn.execute(
        "UPDATE provider_instances SET api_key = ?1 WHERE id = ?2",
        params![api_key, id],
    )?;
    Ok(())
}

/// 更新实例的 alias
pub fn update_alias(conn: &Connection, id: &str, alias: &str) -> Result<()> {
    conn.execute(
        "UPDATE provider_instances SET alias = ?1 WHERE id = ?2",
        params![alias, id],
    )?;
    Ok(())
}
