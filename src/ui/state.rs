// ── UI 状态类型 ──

pub use crate::model::*;

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
    General,
    Appearance,
}

impl SettingsTab {
    pub fn label(&self) -> &'static str {
        match self {
            SettingsTab::Providers => "provider instances",
            SettingsTab::AgentConfigs => "agent configs",
            SettingsTab::General => "general",
            SettingsTab::Appearance => "appearance",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            SettingsTab::Providers => "⊕",
            SettingsTab::AgentConfigs => "⚙",
            SettingsTab::General => "◎",
            SettingsTab::Appearance => "◐",
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
