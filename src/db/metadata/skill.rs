// ── Skill 状态 CRUD ──
//
// 磁盘是技能的唯一真相；skill_states 只存运行状态：
//   status（enabled/disabled）、usage_count、last_run_at。

use rusqlite::{Connection, Result, params, params_from_iter};

/// skill_states 表行
#[derive(Debug, Clone, PartialEq)]
pub struct SkillStateRow {
    pub name: String,
    /// "enabled" | "disabled"
    pub status: String,
    pub usage_count: i64,
    pub last_run_at: Option<String>,
    pub updated_at: String,
}

/// 查询单个技能状态
pub fn get(conn: &Connection, name: &str) -> Result<Option<SkillStateRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, status, usage_count, last_run_at, updated_at
         FROM skill_states WHERE name = ?1",
    )?;
    let mut rows = stmt.query_map(params![name], state_from_row)?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        _ => Ok(None),
    }
}

/// 列出全部状态
pub fn list(conn: &Connection) -> Result<Vec<SkillStateRow>> {
    let mut stmt = conn.prepare(
        "SELECT name, status, usage_count, last_run_at, updated_at
         FROM skill_states ORDER BY name",
    )?;
    let rows = stmt.query_map([], state_from_row)?;
    let mut result = Vec::new();
    for r in rows {
        result.push(r?);
    }
    Ok(result)
}

/// 确保磁盘技能有一条状态记录（默认 enabled，不覆盖已有状态）
pub fn ensure(conn: &Connection, name: &str) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO skill_states (name, status, usage_count, last_run_at, updated_at)
         VALUES (?1, 'enabled', 0, NULL, ?2)",
        params![name, chrono::Local::now().to_rfc3339()],
    )?;
    Ok(())
}

/// 更新技能状态（enabled / disabled）
pub fn set_status(conn: &Connection, name: &str, status: &str) -> Result<()> {
    conn.execute(
        "UPDATE skill_states SET status = ?1, updated_at = ?2 WHERE name = ?3",
        params![status, chrono::Local::now().to_rfc3339(), name],
    )?;
    Ok(())
}

/// 记录一次运行（usage +1，刷新 last_run_at）
pub fn record_run_by_name(conn: &Connection, name: &str) -> Result<()> {
    conn.execute(
        "UPDATE skill_states
         SET usage_count = usage_count + 1, last_run_at = ?1, updated_at = ?1
         WHERE name = ?2",
        params![chrono::Local::now().to_rfc3339(), name],
    )?;
    Ok(())
}

/// 删除状态记录（卸载技能时调用）
pub fn delete(conn: &Connection, name: &str) -> Result<()> {
    conn.execute("DELETE FROM skill_states WHERE name = ?1", params![name])?;
    Ok(())
}

/// 清理磁盘上已不存在技能的状态记录
pub fn prune_missing(conn: &Connection, present: &[String]) -> Result<()> {
    if present.is_empty() {
        conn.execute("DELETE FROM skill_states", [])?;
        return Ok(());
    }
    let placeholders = vec!["?"; present.len()].join(",");
    let sql = format!("DELETE FROM skill_states WHERE name NOT IN ({placeholders})");
    conn.execute(&sql, params_from_iter(present.iter()))?;
    Ok(())
}

fn state_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SkillStateRow> {
    Ok(SkillStateRow {
        name: row.get(0)?,
        status: row.get(1)?,
        usage_count: row.get(2)?,
        last_run_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::with_db;

    const TEST_SKILL: &str = "test_state_skill";

    #[test]
    fn state_roundtrip() {
        with_db(|conn| {
            delete(conn, TEST_SKILL).unwrap();
            ensure(conn, TEST_SKILL).unwrap();
            let s = get(conn, TEST_SKILL).unwrap().expect("state should exist");
            assert_eq!(s.status, "enabled");
            assert_eq!(s.usage_count, 0);

            set_status(conn, TEST_SKILL, "disabled").unwrap();
            assert_eq!(get(conn, TEST_SKILL).unwrap().unwrap().status, "disabled");

            record_run_by_name(conn, TEST_SKILL).unwrap();
            let used = get(conn, TEST_SKILL).unwrap().unwrap();
            assert_eq!(used.usage_count, 1);
            assert!(used.last_run_at.is_some());

            delete(conn, TEST_SKILL).unwrap();
            assert!(get(conn, TEST_SKILL).unwrap().is_none());
        });
    }

    #[test]
    fn prune_removes_missing_skills() {
        with_db(|conn| {
            ensure(conn, "prune_keep").unwrap();
            ensure(conn, "prune_gone").unwrap();
            prune_missing(conn, &["prune_keep".to_string()]).unwrap();
            assert!(get(conn, "prune_keep").unwrap().is_some());
            assert!(get(conn, "prune_gone").unwrap().is_none());
            delete(conn, "prune_keep").unwrap();
        });
    }
}
