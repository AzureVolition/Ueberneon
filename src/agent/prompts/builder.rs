//! Prompt 格式化状态机。
//!
//! 使用方式：
//! ```ignore
//! let prompt = PromptBuilder::new(template)
//!     .with_var("workspace_path", "/path/to/project")
//!     .with_var("tool_list", "ReadFile, Grep, ...")
//!     .build();
//! ```

use std::collections::HashMap;

/// 状态机的当前阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuilderState {
    /// 刚创建，尚未填充任何变量。
    Idle,
    /// 正在收集变量（调用了 with_var）。
    Building,
    /// 已调用 build()，输出已生成。
    Ready,
}

/// Prompt 格式化上下文 —— explore / plan / regular 各自所需的变量集合。
#[derive(Debug, Clone)]
pub struct PromptContext {
    /// 项目工作区路径，如 /Users/xxx/my-project
    pub workspace_path: String,
    /// 项目名称
    pub project_name: String,
    /// 可用工具列表（逗号分隔）
    pub tool_list: String,
    /// 环境信息（OS / shell / 已安装工具等）
    pub env_info: String,
}

impl Default for PromptContext {
    fn default() -> Self {
        Self {
            workspace_path: String::new(),
            project_name: String::new(),
            tool_list: String::new(),
            env_info: format!(
                "OS: {} | Shell: {}",
                std::env::consts::OS,
                std::env::var("SHELL").unwrap_or_else(|_| "unknown".into())
            ),
        }
    }
}

/// Prompt 格式化状态机。
///
/// 管理 `${variable_name}` 占位符的收集和替换。
/// 流程：`new(template)` → `with_var(k, v)`* → `build()`
#[derive(Debug, Clone)]
pub struct PromptBuilder {
    template: String,
    vars: HashMap<String, String>,
    state: BuilderState,
}

impl PromptBuilder {
    /// 创建状态机，进入 Idle 状态。
    pub fn new(template: String) -> Self {
        Self {
            template,
            vars: HashMap::new(),
            state: BuilderState::Idle,
        }
    }

    /// 根据 agent 类型和上下文初始化状态机并预填对应变量。
    ///
    /// - `SubAgent` (explore/plan)：注入工具列表 + 工作区 + 项目名
    /// - 其他（主 agent）：注入工作区 + 项目名
    pub fn init_for(agent_type: &str, ctx: &PromptContext) -> Self {
        let mut builder = Self::new(String::new()) // template 由调用方设置
            .with_var("workspace_path", &ctx.workspace_path)
            .with_var("project_name", &ctx.project_name);

        if agent_type == "SubAgent" {
            builder = builder
                .with_var("tool_list", &ctx.tool_list)
                .with_var("env_info", &ctx.env_info)
                .with_var("workspace_path", &ctx.workspace_path)
                .with_var("project_name", &ctx.project_name);
        }

        builder
    }

    /// 设置模板内容（从 DB 或其他来源读取后调用）。
    pub fn with_template(mut self, template: String) -> Self {
        self.template = template;
        self
    }

    /// 填充一个变量，进入 Building 状态。
    pub fn with_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.state = BuilderState::Building;
        self.vars.insert(key.into(), value.into());
        self
    }

    /// 完成格式化，进入 Ready 状态并返回最终 prompt。
    pub fn build(mut self) -> String {
        self.state = BuilderState::Ready;
        let mut result = self.template.clone();
        for (key, value) in &self.vars {
            result = result.replace(&format!("${{{}}}", key), value);
        }
        result
    }

    /// 查看当前状态。
    pub fn state(&self) -> BuilderState {
        self.state
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_workflow() {
        let prompt = PromptBuilder::new("Hello, ${name}! You are in ${workspace_path}.".into())
            .with_var("name", "World")
            .with_var("workspace_path", "/home/user")
            .build();

        assert_eq!(prompt, "Hello, World! You are in /home/user.");
    }

    #[test]
    fn test_state_transitions() {
        let b = PromptBuilder::new("test".into());
        assert_eq!(b.state(), BuilderState::Idle);

        let b = b.with_var("k", "v");
        assert_eq!(b.state(), BuilderState::Building);

        let _result = b.build();
        // build consumes self, state is now Ready
    }

    #[test]
    fn test_missing_var_unchanged() {
        let prompt = PromptBuilder::new("${a} ${b}".into())
            .with_var("a", "1")
            .build();

        assert_eq!(prompt, "1 ${b}");
    }
}
