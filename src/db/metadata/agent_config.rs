// ── Agent 配置 CRUD ──


use rusqlite::{params, Connection, Result};

/// 数据库行
#[derive(Debug, Clone, PartialEq)]
pub struct AgentConfigRow {
    pub id: String,
    pub name: String,
    pub agent_type: String,       // "InBuilt" | "Custom" | "SubAgent"
    pub provider_instance_id: String,
    pub model: String,
    pub base_url: String,         // 保存时从 provider 自动填充
    pub api_key: String,          // 保存时从 provider instance 自动填充
    pub system_prompt: String,
    pub temperature: f64,
    pub max_tokens: Option<u32>,
    pub tools: String,            // JSON 数组: ["Bash","ReadFile",...]  空数组 = 全部
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug)]
pub enum AgentType {
    InBuilt,
    Custom,
    SubAgent,
}

impl std::fmt::Display for AgentType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AgentType::InBuilt => write!(f, "InBuilt"),
            AgentType::Custom => write!(f, "Custom"),
            AgentType::SubAgent => write!(f, "SubAgent"),
        }
    }
}

/// 插入一条 agent 配置
pub fn insert(conn: &Connection, row: &AgentConfigRow) -> Result<()> {
    conn.execute(
        "INSERT INTO agent_configs (id, name, agent_type, provider_instance_id, model,
         base_url, api_key, system_prompt, temperature, max_tokens, tools, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            row.id, row.name, row.agent_type, row.provider_instance_id, row.model,
            row.base_url, row.api_key, row.system_prompt, row.temperature, row.max_tokens, row.tools,
            row.created_at, row.updated_at,
        ],
    )?;
    Ok(())
}

/// 更新一条 agent 配置
pub fn update(conn: &Connection, row: &AgentConfigRow) -> Result<()> {
    conn.execute(
        "UPDATE agent_configs SET name=?1, agent_type=?2, provider_instance_id=?3, model=?4,
         base_url=?5, api_key=?6, system_prompt=?7, temperature=?8, max_tokens=?9, tools=?10, updated_at=?11
         WHERE id=?12",
        params![
            row.name, row.agent_type, row.provider_instance_id, row.model,
            row.base_url, row.api_key, row.system_prompt, row.temperature, row.max_tokens, row.tools,
            row.updated_at, row.id,
        ],
    )?;
    Ok(())
}

/// 删除一条 agent 配置
pub fn delete(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM agent_configs WHERE id = ?1", params![id])?;
    Ok(())
}

/// 获取单个 agent 配置
pub fn get(conn: &Connection, id: &str) -> Result<Option<AgentConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, agent_type, provider_instance_id, model,
         base_url, api_key, system_prompt, temperature, max_tokens, tools, created_at, updated_at
         FROM agent_configs WHERE id = ?1"
    )?;
    let mut rows = stmt.query_map(params![id], |row| {
        Ok(AgentConfigRow {
            id: row.get(0)?,
            name: row.get(1)?,
            agent_type: row.get(2)?,
            provider_instance_id: row.get(3)?,
            model: row.get(4)?,
            base_url: row.get(5)?,
            api_key: row.get(6)?,
            system_prompt: row.get(7)?,
            temperature: row.get(8)?,
            max_tokens: row.get(9)?,
            tools: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    })?;
    match rows.next() {
        Some(Ok(r)) => Ok(Some(r)),
        _ => Ok(None),
    }
}

/// 列出所有 agent 配置（按 updated_at 降序）
pub fn list_all(conn: &Connection) -> Result<Vec<AgentConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, agent_type, provider_instance_id, model,
         base_url, api_key, system_prompt, temperature, max_tokens, tools, created_at, updated_at
         FROM agent_configs ORDER BY updated_at DESC"
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(AgentConfigRow {
            id: row.get(0)?,
            name: row.get(1)?,
            agent_type: row.get(2)?,
            provider_instance_id: row.get(3)?,
            model: row.get(4)?,
            base_url: row.get(5)?,
            api_key: row.get(6)?,
            system_prompt: row.get(7)?,
            temperature: row.get(8)?,
            max_tokens: row.get(9)?,
            tools: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    })?;
    rows.collect()
}

/// 按 name 查询配置
pub fn get_by_name(conn: &Connection, name: &str) -> Result<Option<AgentConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, agent_type, provider_instance_id, model,
         base_url, api_key, system_prompt, temperature, max_tokens, tools, created_at, updated_at
         FROM agent_configs WHERE name = ?1"
    )?;
    let mut rows = stmt.query_map(params![name], |row| {
        Ok(AgentConfigRow {
            id: row.get(0)?,
            name: row.get(1)?,
            agent_type: row.get(2)?,
            provider_instance_id: row.get(3)?,
            model: row.get(4)?,
            base_url: row.get(5)?,
            api_key: row.get(6)?,
            system_prompt: row.get(7)?,
            temperature: row.get(8)?,
            max_tokens: row.get(9)?,
            tools: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    })?;
    match rows.next() {
        Some(Ok(row)) => Ok(Some(row)),
        _ => Ok(None),
    }
}

/// 按 agent_type 列出配置（按 updated_at 降序）
pub fn list_by_type(conn: &Connection, agent_type: &str) -> Result<Vec<AgentConfigRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, name, agent_type, provider_instance_id, model,
         base_url, api_key, system_prompt, temperature, max_tokens, tools, created_at, updated_at
         FROM agent_configs WHERE agent_type = ?1 ORDER BY updated_at DESC"
    )?;
    let rows = stmt.query_map(params![agent_type], |row| {
        Ok(AgentConfigRow {
            id: row.get(0)?,
            name: row.get(1)?,
            agent_type: row.get(2)?,
            provider_instance_id: row.get(3)?,
            model: row.get(4)?,
            base_url: row.get(5)?,
            api_key: row.get(6)?,
            system_prompt: row.get(7)?,
            temperature: row.get(8)?,
            max_tokens: row.get(9)?,
            tools: row.get(10)?,
            created_at: row.get(11)?,
            updated_at: row.get(12)?,
        })
    })?;
    rows.collect()
}

// ── Agent 配置-工具组关联 ──────────────────────────────────────────────

/// 保存关联（先删后插，事务内）
pub fn save_groups(conn: &Connection, agent_config_id: &str, group_ids: &[String]) -> Result<()> {
    conn.execute("DELETE FROM agent_config_groups WHERE agent_config_id = ?1", params![agent_config_id])?;
    let mut stmt = conn.prepare(
        "INSERT INTO agent_config_groups (agent_config_id, tool_group_id) VALUES (?1, ?2)"
    )?;
    for gid in group_ids {
        stmt.execute(params![agent_config_id, gid])?;
    }
    Ok(())
}

/// 读取关联的工具组 ID 列表
pub fn load_group_ids(conn: &Connection, agent_config_id: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT tool_group_id FROM agent_config_groups WHERE agent_config_id = ?1 ORDER BY tool_group_id"
    )?;
    let rows = stmt.query_map(params![agent_config_id], |row| {
        row.get::<_, String>(0)
    })?;
    rows.collect()
}
