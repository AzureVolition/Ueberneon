// ── UI 状态类型 ──

use crate::agent::AgentMode;
pub use crate::model::*;

/// 对话运行时数据（per-conv，由 bridge 异步写入，UI 读取）
#[derive(Clone)]
pub struct ConversationRuntime {
    pub messages: Vec<UiMessage>,
    pub tick: u64,
    pub agent_handler: Option<crate::agent::AgentHandler>,
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
    pub agent_config_id: Option<String>,
    pub agent_mode: AgentMode,
    /// 累计 token 用量（每次 LLM 交互后累加）
    pub accumulated_usage: crate::model::TokenUsageRecord,
    /// 最近一次 LLM 交互的 token 用量（不回看板展示，预留）
    pub last_loop_usage: Option<crate::model::TokenUsageRecord>,
    /// LLM 请求次数
    pub request_count: u64,
    /// 上下文窗口上限（来自 agent config）
    pub context_window: u32,
}

impl Default for ConversationRuntime {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            tick: 0,
            agent_handler: None,
            cancel_token: None,
            agent_config_id: None,
            agent_mode: AgentMode::Ask,
            accumulated_usage: crate::model::TokenUsageRecord::default(),
            last_loop_usage: None,
            request_count: 0,
            context_window: crate::model::DEFAULT_CONTEXT_WINDOW,
        }
    }
}

/// 侧边栏视图状态
#[derive(Clone, PartialEq)]
pub enum SidebarView {
    ProjectList,
    ConversationList(String),
    Settings(SettingsTab),
}

/// 设置面板标签页
#[derive(Clone, PartialEq, Debug)]
pub enum SettingsTab {
    Providers,
    AgentConfigs,
    SubAgents,
    General,
    Appearance,
    Tools,
    Skills,
    Sql,
}

impl SettingsTab {
    pub fn label(&self) -> &'static str {
        match self {
            SettingsTab::Providers => "provider instances",
            SettingsTab::AgentConfigs => "agent configs",
            SettingsTab::SubAgents => "sub agents",
            SettingsTab::General => "general",
            SettingsTab::Appearance => "appearance",
            SettingsTab::Tools => "tools",
            SettingsTab::Skills => "skills",
            SettingsTab::Sql => "sql",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            SettingsTab::Providers => "⊕",
            SettingsTab::AgentConfigs => "⚙",
            SettingsTab::SubAgents => "⊞",
            SettingsTab::General => "◎",
            SettingsTab::Appearance => "◐",
            SettingsTab::Tools => "⊡",
            SettingsTab::Skills => "◇",
            SettingsTab::Sql => "📋",
        }
    }
}
