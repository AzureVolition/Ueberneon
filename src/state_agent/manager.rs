// ── AgentManager：全局 Agent 缓存 ──
//
// 以 conversation_id 为 key，管理 Agent 的生命周期。
// 使用 remove + register 模式避免锁跨 await 持有。


use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{OnceLock, RwLock};

use super::hook::HookRegister;
use super::prompts::{PromptBuilder, PromptContext};
use super::{Agent, AgentHandler};
use crate::db::metadata::agent_config::AgentConfigRow;
use crate::tools::Registry;
use crate::tools::register_builtins;

use llm::OpenAiProvider;

/// Agent 运行时配置（从 AgentConfigRow 转换而来）
#[derive(Clone)]
pub struct AgentConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub system_prompt: String,
    pub temperature: f64,
    pub max_tokens: Option<u32>,
    pub context_window: u32,       // 上下文窗口上限
    pub agent_type: String,
    pub enabled_tools: Vec<String>,
}

impl AgentConfig {
    /// 从数据库行构建运行配置
    pub fn from_row(row: &AgentConfigRow) -> anyhow::Result<Self> {
        let decoded_key = if !row.api_key.is_empty() {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.decode(row.api_key.as_bytes())
                .ok().and_then(|v| String::from_utf8(v).ok())
                .unwrap_or_default()
        } else {
            anyhow::bail!("api_key is empty")
        };
        Ok(Self {
            model: row.model.clone(),
            base_url: row.base_url.clone(),
            api_key: decoded_key,
            system_prompt: row.system_prompt.clone(),
            temperature: row.temperature,
            max_tokens: row.max_tokens,
            context_window: row.context_window.unwrap_or(crate::model::DEFAULT_CONTEXT_WINDOW),
            agent_type: row.agent_type.clone(),
            enabled_tools: serde_json::from_str(&row.tools).unwrap_or_default(),
        })
    }
}

/// 全局 Agent 管理器
pub struct AgentManager {
    agents: RwLock<HashMap<String, Agent>>,
}

impl AgentManager {
    /// 获取全局单例
    pub fn get() -> &'static Self {
        static INSTANCE: OnceLock<AgentManager> = OnceLock::new();
        INSTANCE.get_or_init(|| {
            AgentManager {
                agents: RwLock::new(HashMap::new()),
            }
        })
    }

    /// 根据 AgentConfig 构建 provider + registry + hooks + Agent
    fn build_agent_inner(
        conversation_id: String,
        project_id: Option<String>,
        cfg: &AgentConfig,
    ) -> Result<Agent, String> {
        tracing::info!(
            target: "agent",
            conversation_id = %conversation_id,
            model = %cfg.model,
            agent_type = %cfg.agent_type,
            tools = cfg.enabled_tools.len(),
            // todo 临时打印
            api_key = %cfg.api_key,
            has_system_prompt = !cfg.system_prompt.is_empty(),
            "building agent"
        );

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
        

        let project_row = crate::db::with_db(|conn| {
            crate::db::metadata::project::get(conn, pid)
                .map_err(|e| format!("db error: {e}"))?
                .ok_or(format!("{} project not found", pid))
        })?;

        let project_path = PathBuf::from(project_row.path);
        register_builtins(&registry, &project_path);

        // 如果配置了启用工具列表，移除未启用的工具
        if !cfg.enabled_tools.is_empty() {
            let all_schemas = registry.schemas();
            let enabled_set: std::collections::HashSet<String> =
                cfg.enabled_tools.iter().cloned().collect();
            for schema in &all_schemas {
                if !enabled_set.contains(&schema.name) {
                    registry.remove_prefix(&schema.name);
                }
            }
        }

        let hook_register = HookRegister::new();

        // 构建 PromptContext（必须在 Agent::new 之前，因为 registry / project_path 会被 move）
        let ctx = PromptContext {
            workspace_path: project_path.display().to_string(),
            project_name: project_row.name.clone(),
            tool_list: registry.schemas().iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            env_info: PromptContext::default().env_info,
        };
        
        let mut agent = Agent::new(
            Box::new(provider),
            registry,
            hook_register,
            project_path,
            project_id,
            conversation_id,
            cfg.temperature,
            cfg.max_tokens,
            cfg.context_window,
            cfg.agent_type.clone(),
        );
        // 优先使用传入的 system_prompt，否则使用 DB 配置中的
        let template =  {
            let s = cfg.system_prompt.trim().to_string();
            if s.is_empty() { super::main_agent::defautlt_main_agent_prompt() } else { s}
        };

        let sp = PromptBuilder::init_for(&cfg.agent_type, &ctx)
            .with_template(template)
            .build();
        agent.init_history(sp);
        
        Ok(agent)
    }

    /// 初始化或获取 Agent。
    ///
    /// - `conv_id = None`：新建对话，生成 id，创建 Agent，注册，返回 (id, handler)。
    /// - `conv_id = Some(id)`：优先从缓存返回；缓存 miss 则从 DB 重建并注册，返回 handler。
    pub fn init_or_get(
        &self,
        conv_id: Option<String>,
        project_id: Option<String>,
        agent_config_id: Option<&str>,
        parent_conversation_id: Option<&str>,
    ) -> Result<(String, AgentHandler), String> {
        match conv_id {
            Some(id) => {
                let handler = self.init(&id)?;
                Ok((id, handler))
            }
            None => {
                let id = crate::db::metadata::conversation::generate_conversation_id();
                // 从 DB 读取 agent 配置
                let agent_config = agent_config_id
                    .map(|ac_id| Self::read_agent_config(ac_id))
                    .unwrap_or_else(|| Err("no agent config selected".to_string()))?;
                tracing::info!(
                    target: "agent",
                    conversation_id = %id,
                    agent_config_id = ?agent_config_id,
                    "creating new conversation"
                );
                // 创建 conversation 数据库行
                crate::db::with_db(|conn| {
                    let pid = project_id.as_deref().unwrap_or(crate::db::DEFAULT_PROJECT_ID);
                    if let Err(e) = conn.execute(
                        "INSERT INTO conversations (id, project_id, parent_conversation_id, title, updated_at, created_at, agent_config_id) VALUES (?1, ?2, ?3, '', ?4, ?5, ?6)",
                        rusqlite::params![id, pid, parent_conversation_id, chrono::Local::now().to_rfc3339(), chrono::Local::now().to_rfc3339(), agent_config_id],
                    ) { tracing::error!(target:"db", error=%e, "insert conversation"); }
                });
                let agent =
                    Self::build_agent_inner(id.clone(), project_id, &agent_config)?;
                let handler = agent.handler.clone();
                self.agents.write().expect("agents lock poisoned").insert(id.clone(), agent);
                Ok((id, handler))
            }
        }
    }

    pub fn init(&self, id: &str) -> Result<AgentHandler, String> {
        {
            let agents = self.agents.read().expect("agents lock poisoned");
            if let Some(agent) = agents.get(id) {
                return Ok(agent.handler.clone());
            }
        }
        // 从 DB 重建
        let (conv, msgs) = crate::db::with_db(|conn| -> Result<_, String> {
            let exists = crate::db::metadata::conversation::get(conn, &id)
                .map_err(|e| format!("db error: {e}"))?
                .is_some();
            if !exists {
                return Err(format!("conversation {id} not found"));
            }
            let conv = crate::db::metadata::conversation::get(conn, &id)
                .map_err(|e| format!("db error: {e}"))?
                .ok_or(format!("conversation not found"))?;
            let msgs = crate::db::metadata::message::list_as_llm_messages(conn, &id)
                .unwrap_or_default();
            Ok((conv, msgs))
        })?;

        // 读取 conversation 关联的 agent 配置
        let agent_config = conv.agent_config_id
            .as_deref()
            .map(Self::read_agent_config)
            .unwrap_or_else(|| Err(format!("no agent config for conversation {id}")))?;

        let mut agent = Self::build_agent_inner(id.to_string(), Some(conv.project_id), &agent_config)?;
        agent.messages.extend(msgs);
        let handler = agent.handler.clone();
        self.agents.write().expect("agents lock poisoned").insert(id.to_string(), agent);
        Ok(handler)
    }

    /// 根据 agent_config_id 从 DB 读取配置并转为 AgentConfig
    fn read_agent_config(agent_config_id: &str) -> Result<AgentConfig, String> {
        tracing::info!(
            target: "agent",
            agent_config_id = %agent_config_id,
            "reading agent config from db"
        );
        let row = crate::db::with_db(|conn| {
            crate::db::metadata::agent_config::get(conn, agent_config_id)
                .map_err(|e| format!("db error: {e}"))?
                .ok_or_else(|| format!("agent config {agent_config_id} not found"))
        })?;

        // 如果是 SubAgent 且未配置 model，尝试使用默认设置
        if row.agent_type == "SubAgent" && row.model.is_empty() {
            let s = crate::settings::get();
            let default_inst_id = s.general.default_subagent_provider_instance_id;
            let default_model = s.general.default_subagent_model;

            if !default_inst_id.is_empty() && !default_model.is_empty() {
                let (base_url, api_key) = crate::db::with_db(|conn| {
                    let inst = crate::db::metadata::provider_instance::get(conn, &default_inst_id)
                        .ok().flatten();
                    let (raw_key, prov_id) = match inst {
                        Some(ref i) => (i.api_key.clone(), i.provider_id.clone()),
                        None => (String::new(), String::new()),
                    };
                    let url = crate::db::metadata::provider::get(conn, &prov_id)
                        .ok().flatten().map(|p| p.base_url).unwrap_or_default();
                    (url, raw_key)
                });
                let decoded_key = if !api_key.is_empty() {
                    use base64::Engine;
                    base64::engine::general_purpose::STANDARD.decode(api_key.as_bytes())
                        .ok().and_then(|v| String::from_utf8(v).ok())
                        .unwrap_or_default()
                } else {
                    String::new()
                };
                return Ok(AgentConfig {
                    model: default_model,
                    base_url,
                    api_key: decoded_key,
                    system_prompt: row.system_prompt,
                    temperature: row.temperature,
                    max_tokens: row.max_tokens,
                    context_window: row.context_window.unwrap_or(crate::model::DEFAULT_CONTEXT_WINDOW),
                    agent_type: row.agent_type,
                    enabled_tools: serde_json::from_str(&row.tools).unwrap_or_default(),
                });
            }
            // 没有默认配置 → 明确报错
            return Err(format!(
                "子 Agent '{}' 未配置 Provider/Model。请在 设置 > Sub Agents 中配置默认 SubAgent Provider 和 Model",
                row.name
            ));
        }

        AgentConfig::from_row(&row).map_err(|e| format!("{e}"))
    }

    

    /// 从缓存移除并返回 Agent（ownership 转移，适合取出后异步执行）。
    pub fn remove(&self, id: &str) -> Option<Agent> {
        self.agents.write().expect("agents lock poisoned").remove(id)
    }

    /// 将 Agent 注册回缓存。
    pub fn register(&self, agent: Agent) {
        let id = agent.conversation_id.clone();
        self.agents.write().expect("agents lock poisoned").insert(id, agent);
    }

    /// 检查对话是否存在（缓存 or DB）。
    pub fn exists(&self, id: &str) -> Result<bool, String> {
        if self.agents.read().expect("agents lock poisoned").contains_key(id) {
            return Ok(true);
        }
        let found = crate::db::with_db(|conn| {
            crate::db::metadata::conversation::get(conn, id)
                .map_err(|e| format!("db error: {e}"))
        })?
        .is_some();
        Ok(found)
    }

    /// 读取缓存的 Agent handler（不取出所有权，适合只读场景）。
    pub fn get_agent(&self, id: &str) -> Option<AgentHandler> {
        self.agents.read().expect("agents lock poisoned").get(id).map(|a| a.handler.clone())
    }
}
