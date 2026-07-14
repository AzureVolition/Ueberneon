// checkable_tool.rs —— 统一的校验 + 执行流程。
//
// CheckableTool 继承 Tool，提供标准入口 checked_execute：
//   check() → Decision::Deny/Ask → run()
//
// Registry 存储 CheckableTool，Agent 调用 checked_execute。

use llm::tool::{Tool, ToolContext, ToolResult};
use crate::permission::Decision;

/// 带校验的工具执行流程。
#[async_trait::async_trait]
pub trait CheckableTool: Tool {
    /// 前置校验逻辑。返回 Decision::Allow 放行。
    fn check(&self, ctx: &ToolContext, args: &serde_json::Value) -> Decision;

    
    /// 完整执行入口 = check() → run()。
    async fn checked_execute(&self, ctx: &ToolContext, args: &serde_json::Value) -> Result<ToolResult, String> {
        match self.check(ctx, args) {
            Decision::Allow => {}
            Decision::Ask => todo!("needs approval"),
            Decision::Deny(msg) => return Err(msg),
        }
        self.execute(ctx, args).await
    }
}
