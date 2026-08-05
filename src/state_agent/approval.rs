// ── ApprovalGate：审批策略接口 ──
//
// gate 只做决策，返回 permission::Decision（与 pre_check 返回类型兼容）：
//   Allow → 直接执行；Deny(msg) → 拒绝；Ask → 交给 execute 的审批管道（等 UI 注入）。
// 审批管道（mpsc (tool_call_id, approved) + 子线程转发）完全由 execute 管理，
// gate 不参与。
//
// 链式组合（ApprovalChain）：按序执行，Deny 短路、Ask 直达、全 Allow 放行。
// 场景差异只体现在链的组成：
//   主对话（交互）→ ApprovalChain::new([UserApprovalGate])
//   子 Agent（非交互）→ ApprovalChain::new([AutoDenyApprovalGate])

use crate::permission::Decision;

/// 审批策略接口（纯决策，不管理审批管道）
pub trait ApprovalGate: Send + Sync {
    fn decide(&self, tool_name: &str, args: &serde_json::Value) -> Decision;
}

/// 交互场景：Ask 一律交给 execute 的审批管道（UI 审批卡注入 (tool_call_id, approved)）
pub struct UserApprovalGate;

impl ApprovalGate for UserApprovalGate {
    fn decide(&self, _tool_name: &str, _args: &serde_json::Value) -> Decision {
        Decision::Ask
    }
}

/// 非交互场景（子 Agent 便捷路径等）：Ask 一律自动拒绝，不等待。
pub struct AutoDenyApprovalGate;

impl ApprovalGate for AutoDenyApprovalGate {
    fn decide(&self, tool_name: &str, _args: &serde_json::Value) -> Decision {
        Decision::Deny(format!(
            "{tool_name} needs approval, but no interactive approval is available"
        ))
    }
}

/// 策略链：按序执行，Deny 短路，Ask 直达，全 Allow 放行
pub struct ApprovalChain {
    gates: Vec<Box<dyn ApprovalGate>>,
}

impl ApprovalChain {
    pub fn new(gates: Vec<Box<dyn ApprovalGate>>) -> Self {
        Self { gates }
    }
}

impl ApprovalGate for ApprovalChain {
    fn decide(&self, tool_name: &str, args: &serde_json::Value) -> Decision {
        for gate in &self.gates {
            match gate.decide(tool_name, args) {
                Decision::Deny(msg) => return Decision::Deny(msg),
                Decision::Ask => return Decision::Ask,
                Decision::Allow => {}
            }
        }
        Decision::Allow
    }
}
