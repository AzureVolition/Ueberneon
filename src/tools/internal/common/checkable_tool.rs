// checkable_tool.rs —— 统一的校验 + 执行流程。
//
// CheckableTool 继承 Tool，提供标准入口 checked_execute：
//   check() → Decision::Deny/Ask → run()
//
// Registry 存储 CheckableTool，Agent 调用 checked_execute。

use crate::agent::{Tool, AgentContext, ToolResult};
use crate::permission::Decision;

/// 带校验的工具执行流程。
#[async_trait::async_trait]
pub trait CheckableTool: Tool {
    /// 前置校验逻辑。返回 Decision::Allow 放行。
    fn check(&self, ctx: &AgentContext, args: &serde_json::Value) -> Decision;

    /// 仅执行校验，不执行工具。Agent 用此方法在 execute 前分流 Ask。
    fn pre_check(&self, ctx: &AgentContext, args: &serde_json::Value) -> Decision {
        self.check(ctx, args)
    }

    /// 完整执行入口 = check() → run()。
    /// Decision::Ask 会返回错误（交互式审批需由 Agent 层通过 pre_check + execute 分流处理）。
    async fn checked_execute(&self, ctx: &AgentContext, args: &serde_json::Value) -> Result<ToolResult, String> {
        match self.check(ctx, args) {
            Decision::Allow => {}
            Decision::Ask => {
                return Err("NEEDS_APPROVAL: interactive approval required".into());
            }
            Decision::Deny(msg) => return Err(msg),
        }
        self.execute(ctx, args).await
    }
}
