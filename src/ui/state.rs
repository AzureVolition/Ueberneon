// ── UI 状态类型 ──
//
// 核心数据模型已迁移到 crate::model；此处保留 UI 专用类型并通过 pub use 重新导出。

use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};

// 重新导出核心数据模型（保持现有 use crate::ui::state::* 兼容）
pub use crate::model::*;

/// 侧边栏视图状态
#[derive(Clone, PartialEq)]
pub enum SidebarView {
    /// 显示项目列表
    ProjectList,
    /// 显示指定项目的对话列表
    ConversationList(String),
}

/// 应用配置
#[derive(Clone)]
pub struct AppConfig {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub temperature: f64,
    pub max_tokens: u32,
    pub agent_mode: String,
}

/// 待审批的工具调用（仅在桥接层使用，不持久化）
#[derive(Clone)]
pub struct PendingApproval {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub reason: String,
}
