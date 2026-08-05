// ── ApprovalGate：审批策略接口 ──
//
// gate 只做决策，返回 permission::Decision（与 pre_check 返回类型兼容）：
//   Allow → 直接执行；Deny(msg) → 拒绝；Ask → 交给 execute 的审批管道（等 UI 注入）。
// 审批管道（mpsc (tool_call_id, approved) + 子线程转发）完全由 execute 管理，
// gate 不参与。
//
// gate 是纯工具级静态门：只依据工具名与只读标记判定「这个工具调用是否需要询问」——
//   只读工具 / 非执行类工具 → Allow（安全通过）；
//   执行类工具（bash / kill_shell / task）→ Ask（询问）。
// 命令级危险判定（rm -rf、force push 等）由工具自身 pre_check 负责，在 execute
// 调用点与 gate 决策合并（Deny 优先）。Unrestrained 的 Ask→Allow 降级由
// running.rs 的 apply_agent_mode 统一兜底，gate 不做模式外推。
//
// 链式组合（ApprovalChain）：按序执行，Deny 短路、Ask 直达、全 Allow 放行。
// 场景差异只体现在链的组成：
//   主对话（交互）→ ApprovalChain::new([UserApprovalGate])
//   子 Agent（非交互）→ ApprovalChain::new([AutoDenyApprovalGate])

use crate::permission::Decision;

/// 执行类工具名单：这些工具调用一律询问（Ask）。
/// 只读标记（`#[tool(read_only)]`）优先——名单内工具若被标记只读仍直接放行。
const DANGEROUS_TOOLS: &[&str] = &["bash", "kill_shell", "task"];

/// 审批策略接口（纯决策，不管理审批管道）
pub trait ApprovalGate: Send + Sync {
    /// 决策。`tool_name` 为工具名，`read_only` 为工具注册时的只读标记
    /// （`#[tool(read_only)]`）。
    fn decide(&self, tool_name: &str, args: &serde_json::Value, read_only: bool) -> Decision;
}

/// 交互场景：工具级静态门——危险（执行类）工具调用询问，安全工具直接通过。
/// 只读工具与非执行类工具（文件编辑、搜索、索引等）→ Allow；
/// 执行类工具（bash / kill_shell / task）→ Ask。
pub struct UserApprovalGate;

impl ApprovalGate for UserApprovalGate {
    fn decide(&self, tool_name: &str, _args: &serde_json::Value, read_only: bool) -> Decision {
        if read_only || !DANGEROUS_TOOLS.contains(&tool_name) {
            Decision::Allow
        } else {
            Decision::Ask
        }
    }
}

/// 非交互场景（子 Agent 便捷路径等）：自动拒绝，不等待。
pub struct AutoDenyApprovalGate;

impl ApprovalGate for AutoDenyApprovalGate {
    fn decide(&self, tool_name: &str, _args: &serde_json::Value, _read_only: bool) -> Decision {
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
    fn decide(&self, tool_name: &str, args: &serde_json::Value, read_only: bool) -> Decision {
        for gate in &self.gates {
            match gate.decide(tool_name, args, read_only) {
                Decision::Deny(msg) => return Decision::Deny(msg),
                Decision::Ask => return Decision::Ask,
                Decision::Allow => {}
            }
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UserApprovalGate：只读工具与非执行类写工具直接通过，执行类工具询问。
    #[test]
    fn user_gate_safe_allow_dangerous_ask() {
        let g = UserApprovalGate;
        // 只读工具
        assert_eq!(g.decide("read_file", &serde_json::json!({}), true), Decision::Allow);
        assert_eq!(g.decide("ls", &serde_json::json!({}), true), Decision::Allow);
        // 非执行类写工具（文件编辑等）→ 安全通过
        assert_eq!(g.decide("edit_file", &serde_json::json!({}), false), Decision::Allow);
        assert_eq!(g.decide("write_file", &serde_json::json!({}), false), Decision::Allow);
        assert_eq!(g.decide("multi_edit", &serde_json::json!({}), false), Decision::Allow);
        // 执行类工具 → 询问
        assert_eq!(g.decide("bash", &serde_json::json!({}), false), Decision::Ask);
        assert_eq!(g.decide("kill_shell", &serde_json::json!({}), false), Decision::Ask);
        assert_eq!(g.decide("task", &serde_json::json!({}), false), Decision::Ask);
    }

    /// 只读标记优先于执行类名单：名单内工具若被标记只读仍直接放行。
    #[test]
    fn read_only_beats_dangerous_list() {
        let g = UserApprovalGate;
        assert_eq!(g.decide("bash", &serde_json::json!({}), true), Decision::Allow);
    }

    /// AutoDenyApprovalGate：只读标记不影响自动拒绝（子 Agent 非交互）。
    #[test]
    fn auto_deny_ignores_read_only() {
        let g = AutoDenyApprovalGate;
        assert!(matches!(
            g.decide("read_file", &serde_json::json!({}), true),
            Decision::Deny(_)
        ));
        assert!(matches!(
            g.decide("bash", &serde_json::json!({}), false),
            Decision::Deny(_)
        ));
    }

    /// ApprovalChain：read_only 透传到各 gate。
    #[test]
    fn chain_forwards_read_only() {
        let chain = ApprovalChain::new(vec![Box::new(UserApprovalGate)]);
        assert_eq!(chain.decide("read_file", &serde_json::json!({}), true), Decision::Allow);
        assert_eq!(chain.decide("edit_file", &serde_json::json!({}), false), Decision::Allow);
        assert_eq!(chain.decide("bash", &serde_json::json!({}), false), Decision::Ask);
    }
}
