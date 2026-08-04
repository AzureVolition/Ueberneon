// ── ApprovalGate：审批策略接口（B2 形态） ──
//
// 从"阻塞等待用户"改为"非阻塞会话"：
//   gate.start() 立即返回裁决（Allow/Deny）或创建会话（Session）；
//   会话把 oneshot receiver 交给驱动者（bridge），由驱动者决定何时/如何等待用户，
//   拿到结果后调 AgentRun::resolve_approval 恢复执行。
// 这使审批成为真正的挂起点：可超时、可放弃、可序列化（为断点铺路）。

use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use super::PendingApproval;

/// 审批上下文（Ask 分支构造，注入给每个策略）
pub struct ApprovalCtx {
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub reason: String,
    pub cancel: CancellationToken,
    /// 用户审批通道：Some(sender) 时 UI 显示审批卡，用户点选后经 oneshot 回传
    pub approval_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
}

/// 审批裁决：要么立即定，要么创建会话交驱动者
pub enum GateOutcome {
    Allow,
    Deny(String),
    /// 需要人工：会话已建立，UI 显示审批卡，驱动者等 result_rx
    Session {
        req: PendingApproval,
        result_rx: tokio::sync::oneshot::Receiver<bool>,
    },
}

/// 审批策略接口（非阻塞：立即返回裁决或会话，不自己 await 用户）
pub trait ApprovalGate: Send + Sync {
    fn start(&self, ctx: &ApprovalCtx) -> GateOutcome;
}

/// 用户弹窗审批：建立会话，UI 点选后经 oneshot 回传
pub struct UserApprovalGate;

impl ApprovalGate for UserApprovalGate {
    fn start(&self, ctx: &ApprovalCtx) -> GateOutcome {
        let (tx, rx) = tokio::sync::oneshot::channel();
        *ctx.approval_tx.lock().expect("approval_tx lock poisoned") = Some(tx);
        GateOutcome::Session {
            req: PendingApproval {
                call_id: ctx.call_id.clone(),
                tool_name: ctx.tool_name.clone(),
                args: ctx.args.clone(),
                reason: ctx.reason.clone(),
            },
            result_rx: rx,
        }
    }
}

/// 策略链：按序执行，Deny 短路，Session 直达驱动者
pub struct ApprovalChain {
    gates: Vec<Box<dyn ApprovalGate>>,
}

impl ApprovalChain {
    pub fn new(gates: Vec<Box<dyn ApprovalGate>>) -> Self {
        Self { gates }
    }
}

impl ApprovalGate for ApprovalChain {
    fn start(&self, ctx: &ApprovalCtx) -> GateOutcome {
        for gate in &self.gates {
            match gate.start(ctx) {
                GateOutcome::Deny(msg) => return GateOutcome::Deny(msg),
                GateOutcome::Session { req, result_rx } => {
                    return GateOutcome::Session { req, result_rx };
                }
                GateOutcome::Allow => {}
            }
        }
        GateOutcome::Allow
    }
}
