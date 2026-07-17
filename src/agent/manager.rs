// ── AgentManager：全局 Agent 缓存 ──
//
// 以 conversation_id 为 key，管理 Agent 的生命周期。
// 使用 remove + register 模式避免锁跨 await 持有。


use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock, RwLock};

use super::hook::HookRegister;
use super::{ActionMode, Agent, AgentMode};
use crate::model::Project;
use crate::tools::Registry;
use crate::tools::register_builtins;

use llm::OpenAiProvider;

/// 全局 Agent 配置
#[derive(Clone)]
pub struct AgentConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
}

static GLOBAL_CONFIG: OnceLock<RwLock<AgentConfig>> = OnceLock::new();

/// 初始化/更新全局 Agent 配置。首次调用设置锁，后续调用更新内部值。
pub fn init_global_config(config: AgentConfig) {
    let lock = GLOBAL_CONFIG.get_or_init(|| RwLock::new(config.clone()));
    if let Ok(mut guard) = lock.write() {
        *guard = config;
    }
}

fn global_config() -> AgentConfig {
    let guard = GLOBAL_CONFIG
        .get()
        .expect("AgentConfig not initialized")
        .read()
        .expect("AgentConfig lock poisoned");
    (*guard).clone()
}

/// 全局 Agent 管理器
pub struct AgentManager {
    agents: HashMap<String, Agent>,
}

impl AgentManager {
    /// 获取全局单例
    pub fn get() -> &'static Mutex<Self> {
        static INSTANCE: OnceLock<Mutex<AgentManager>> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            Mutex::new(AgentManager {
                agents: HashMap::new(),
            })
        })
    }

    /// 内部：根据全局配置构建 provider + registry + hooks + Agent
    fn build_agent_inner(
        conversation_id: String,
        system_prompt: Option<&str>,
        project_id: Option<String>,
    ) -> Result<Agent, String> {
        let cfg = global_config();
        let provider = OpenAiProvider::new(
            cfg.model.clone(),
            cfg.base_url.clone(),
            cfg.model.clone(),
            cfg.api_key.clone(),
            None,
            false,
            None,
        )
        .map_err(|e| format!("provider error: {e}"))?;

        let registry = Registry::new();
        let pid = project_id.as_deref().unwrap_or(crate::db::DEFAULT_PROJECT_ID);
        

        let conn = crate::db::get_db().lock().map_err(|e| format!("failed to get db conn: {e}"))?;
        let project_row = crate::db::metadata::project::get(&conn, pid)
            .map_err(|e| format!("db error: {e}"))?
            .ok_or(format!("{} project not found", pid))?;

        let project_path = PathBuf::from(project_row.path);
        register_builtins(&registry, &project_path);
        let hook_register = HookRegister::new();

        let mut agent = Agent::new(
            Box::new(provider),
            registry,
            hook_register,
            ActionMode::Regular,
            AgentMode::Ask,
            project_path,
            project_id,
            conversation_id,
        );
        if let Some(system_prompt) = system_prompt {
            agent.init_history(system_prompt.to_string());
        }
        Ok(agent)
    }

    /// 初始化或获取 Agent。
    ///
    /// - `conv_id = None`：新建对话，生成 id，创建 Agent，注册，返回 id。
    /// - `conv_id = Some(id)`：优先从缓存返回；缓存 miss 则从 DB 重建并注册。
    pub fn init_or_get(
        &mut self,
        conv_id: Option<String>,
        system_prompt: &str,
        project_id: Option<String>,
    ) -> Result<String, String> {
        match conv_id {
            Some(id) => {
                self.init(&id)?;
                Ok(id)
            }
            None => {
                let id = crate::db::metadata::conversation::generate_conversation_id();
                // 创建 conversation 数据库行（用已生成的 id，不调用 create 避免二次生成）
                if let Ok(conn) = crate::db::get_db().lock() {
                    let pid = project_id.as_deref().unwrap_or(crate::db::DEFAULT_PROJECT_ID);
                    conn.execute(
                        "INSERT INTO conversations (id, project_id, title, updated_at) VALUES (?1, ?2, '', ?3)",
                        rusqlite::params![id, pid, chrono::Local::now().to_rfc3339()],
                    ).ok();
                }
                let agent =
                    Self::build_agent_inner(id.clone(), Some(system_prompt), project_id)?;
                self.agents.insert(id.clone(), agent);
                Ok(id)
            }
        }
    }

    fn init(&mut self, id: &str) -> Result<(), String> {
        if self.agents.contains_key(id) {
            return Ok(());
        }
        // 从 DB 重建
        let conn = crate::db::get_db()
            .lock()
            .map_err(|e| format!("db error: {e}"))?;
        let exists = crate::db::metadata::conversation::get(&conn, &id)
            .map_err(|e| format!("db error: {e}"))?
            .is_some();
        if !exists {
            return Err(format!("conversation {id} not found"));
        }
        let conv = crate::db::metadata::conversation::get(&conn, &id)
            .map_err(|e| format!("db error: {e}"))?.ok_or(format!("conversation not found"))?;
        
        let msgs = crate::db::metadata::message::list_as_llm_messages(&conn, &id)
            .unwrap_or_default();
        drop(conn);

        let mut agent = Self::build_agent_inner(id.to_string(), None, Some(conv.project_id))?;
        agent.messages.extend(msgs);
        self.agents.insert(id.to_string(), agent);
        Ok(())
    }
    

    /// 从缓存移除并返回 Agent（ownership 转移，适合取出后异步执行）。
    pub fn remove(&mut self, id: &str) -> Option<Agent> {
        self.init(id).ok();
        self.agents.remove(id)
    }

    /// 将 Agent 注册回缓存。
    pub fn register(&mut self, agent: Agent) {
        let id = agent.conversation_id.clone();
        self.agents.insert(id, agent);
    }

    /// 检查对话是否存在（缓存 or DB）。
    pub fn exists(&self, id: &str) -> Result<bool, String> {
        if self.agents.contains_key(id) {
            return Ok(true);
        }
        let conn = crate::db::get_db()
            .lock()
            .map_err(|e| format!("db error: {e}"))?;
        let found = crate::db::metadata::conversation::get(&conn, id)
            .map_err(|e| format!("db error: {e}"))?
            .is_some();
        Ok(found)
    }

    /// 读取缓存的 Agent（不取出所有权，适用于同步只读场景）。
    pub fn get_agent(&self, id: &str) -> Option<&Agent> {
        self.agents.get(id)
    }
}
