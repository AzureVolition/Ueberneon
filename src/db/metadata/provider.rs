// ── Provider CRUD ──

use rusqlite::{Connection, Result, params};

/// 数据库行
#[derive(Debug, Clone)]
pub struct ProviderRow {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub base_url: String,
    pub models_url: String,
    pub balance_url: String,
    pub context_window: u32,
    pub is_preset: bool,
}

/// 插入一个 provider（幂等：已存在则忽略）
pub fn insert(conn: &Connection, row: &ProviderRow) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO providers (id, name, kind, base_url, models_url, balance_url, context_window, is_preset)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![row.id, row.name, row.kind, row.base_url, row.models_url, row.balance_url, row.context_window, row.is_preset as i32],
    )?;
    Ok(())
}

/// 删除 provider（及其关联模型）
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM providers WHERE id = ?1", params![id])?;
    Ok(())
}

/// 获取单个 provider
pub fn get(conn: &Connection, id: &str) -> Result<Option<ProviderRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, base_url, models_url, balance_url, context_window, is_preset FROM providers WHERE id = ?1"
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        Ok(ProviderRow {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            base_url: row.get(3)?,
            models_url: row.get(4)?,
            balance_url: row.get(5)?,
            context_window: row.get::<_, i32>(6)? as u32,
            is_preset: row.get::<_, i32>(7)? != 0,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        _ => Ok(None),
    }
}

/// 列出所有 provider
pub fn list_all(conn: &Connection) -> Result<Vec<ProviderRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, kind, base_url, models_url, balance_url, context_window, is_preset FROM providers ORDER BY is_preset DESC, name ASC"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(ProviderRow {
            id: row.get(0)?,
            name: row.get(1)?,
            kind: row.get(2)?,
            base_url: row.get(3)?,
            models_url: row.get(4)?,
            balance_url: row.get(5)?,
            context_window: row.get::<_, i32>(6)? as u32,
            is_preset: row.get::<_, i32>(7)? != 0,
        })
    })?;
    rows.collect()
}

// ── 模型列表 ──

/// 获取 provider 的模型列表
pub fn list_models(conn: &Connection, provider_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT model_name FROM provider_models WHERE provider_id = ?1 ORDER BY model_name ASC",
    )?;
    let rows = stmt.query_map(params![provider_id], |row| row.get(0))?;
    rows.collect()
}

/// 替换 provider 的模型列表（先删后插）
pub fn replace_models(conn: &Connection, provider_id: &str, models: &[String]) -> Result<()> {
    conn.execute(
        "DELETE FROM provider_models WHERE provider_id = ?1",
        params![provider_id],
    )?;
    let mut stmt = conn.prepare(
        "INSERT OR IGNORE INTO provider_models (provider_id, model_name) VALUES (?1, ?2)",
    )?;
    for m in models {
        stmt.execute(params![provider_id, m])?;
    }
    Ok(())
}
