// ── UI 状态类型 ──

pub use crate::model::*;

/// 侧边栏视图状态
#[derive(Clone, PartialEq)]
pub enum SidebarView {
    ProjectList,
    ConversationList(String),
    Settings,
}


/// 待审批的工具调用
#[derive(Clone)]
pub struct PendingApproval {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub reason: String,
}
