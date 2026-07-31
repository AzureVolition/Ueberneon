// ── ApprovalGate：审批策略接口 ──
//
// 把"Ask 决策之后怎么走审批"从主循环里抽出来，变成可替换的策略链。
// 与 permission::Decision 统一：Allow=放行，Deny(msg)=拒绝，Ask=无法定论（交给链中下一个）。
// 默认链只含 UserApprovalGate（弹窗让用户点），将来可插入规则/超时/自动放行等策略。

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::permission::Decision;

/// 审批上下文（调用方在 Ask 分支构造，注入给每个策略）
pub struct ApprovalCtx {
    pub tool_name: String,
    pub args: serde_json::Value,
    pub reason: String,
    pub cancel: CancellationToken,
    /// 用户审批通道：Some(sender) 时 UI 显示审批卡，用户点选后经 oneshot 回传
    pub approval_tx: Arc<Mutex<Option<tokio::sync::oneshot::Sender<bool>>>>,
}

/// 审批策略接口
#[async_trait]
pub trait ApprovalGate: Send + Sync {
    /// - `Decision::Allow` → 放行执行
    /// - `Decision::Deny(msg)` → 拒绝（msg 为 "cancelled by user" 时调用方终止循环）
    /// - `Decision::Ask` → 本策略无法定论，交给链中下一个策略
    async fn request(&self, ctx: &ApprovalCtx) -> Decision;
}

/// 用户弹窗审批（现有行为：把 Ask 分支的 oneshot 审批流程搬进来）
pub struct UserApprovalGate;

#[async_trait]
impl ApprovalGate for UserApprovalGate {
    async fn request(&self, ctx: &ApprovalCtx) -> Decision {
        let (atx, arx) = tokio::sync::oneshot::channel();
        *ctx.approval_tx.lock().expect("approval_tx lock poisoned") = Some(atx);

        let approval = tokio::select! {
            _ = ctx.cancel.cancelled() => None,
            r = arx => r.ok(),
        };

        match approval {
            Some(true) => Decision::Allow,
            Some(false) => Decision::Deny(format!("denied by user: {}", ctx.reason)),
            None => Decision::Deny("cancelled by user".into()),
        }
    }
}

/// 策略链：按序执行，Deny 短路，Ask 继续下一个，最后归总
pub struct ApprovalChain {
    gates: Vec<Box<dyn ApprovalGate>>,
}

impl ApprovalChain {
    pub fn new(gates: Vec<Box<dyn ApprovalGate>>) -> Self {
        Self { gates }
    }
}

#[async_trait]
impl ApprovalGate for ApprovalChain {
    async fn request(&self, ctx: &ApprovalCtx) -> Decision {
        let mut saw_ask = false;
        for gate in &self.gates {
            match gate.request(ctx).await {
                Decision::Deny(msg) => return Decision::Deny(msg),
                Decision::Ask => saw_ask = true,
                Decision::Allow => {}
            }
        }
        if saw_ask {
            Decision::Ask
        } else {
            Decision::Allow
        }
    }
}
