// ── UI 状态类型 ──

pub use crate::model::*;

/// 对话运行时数据（per-conv，由 bridge 异步写入，UI 读取）
#[derive(Clone)]
pub struct ConversationRuntime {
    pub messages: Vec<UiMessage>,
    pub tick: u64,
    pub agent_handler: Option<crate::agent::AgentHandler>,
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
}

impl Default for ConversationRuntime {
    fn default() -> Self {
        Self {
            messages: Vec::new(),
            tick: 0,
            agent_handler: None,
            cancel_token: None,
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
            SettingsTab::Sql => "📋",
        }
    }
}


/// 待审批的工具调用
#[derive(Clone)]
pub struct PendingApproval {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub reason: String,
}
