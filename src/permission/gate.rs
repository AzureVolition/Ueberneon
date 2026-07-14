// gate.rs —— Policy 组合器 + Gate 审批层。
//
// Policy 将多个 Check 组合为一个规则集，按固定优先级评估：
//   Deny > Ask > Allow > writer_fallback
//
// Gate 包装 Policy 和可选的 Approver，是 Agent 在 execute 前调用的入口。

use crate::permission::{Decision, Check, bash_decompose, extract_subject, extract_subjects};
use llm::tool::AgentMode;

// ── Policy ───────────────────────────────────────────────────────────────────

/// 组合多个 Check 为一个策略。
///
/// Policy 本身是纯逻辑的（无 I/O），
/// 评估时按优先级遍历：Deny > Ask > Allow > writer_fallback。
///
/// 构造示例：
/// ```ignore
/// let policy = Policy::new(Decision::Ask, vec![
///     Box::new(DenySystemPaths),
///     Box::new(ReadOnlyBashClassifier),
/// ]);
/// ```
pub struct Policy {
    /// 写工具的 fallback 决策（当没有 Check 匹配时）。
    /// 只读工具始终 fallback 到 Allow。
    writer_fallback: Decision,
    /// 所有检查，按添加顺序评估。
    /// **优先级不依赖顺序**——Deny 总是优先于 Ask，Ask 总是优先于 Allow。
    checks: Vec<Box<dyn Check>>,
}

impl Policy {
    /// 创建一个新策略。
    ///
    /// `writer_fallback` 是写工具无规则匹配时的默认行为：
    /// - `Decision::Ask`（推荐）— 需要用户/Guardian 确认
    /// - `Decision::Allow` — 自动允许（不推荐写入）
    /// - `Decision::Deny("denied".into())` — 自动拒绝
    ///
    /// `checks` 是 Check 列表，每次评估全部遍历。
    pub fn new(writer_fallback: Decision, checks: Vec<Box<dyn Check>>) -> Self {
        Self {
            writer_fallback,
            checks,
        }
    }

    /// 返回 writer fallback 决策（供 Gate 在只读检查时覆盖）。
    pub fn writer_fallback(&self) -> Decision {
        self.writer_fallback.clone()
    }

    /// 添加一个 Check 到策略末尾。
    pub fn add(&mut self, check: Box<dyn Check>) {
        self.checks.push(check);
    }

    /// 对单 subject 工具调用进行评估。
    ///
    /// 遍历所有 Check，按 Deny > Ask > Allow > fallback 返回最终决策。
    pub fn evaluate(&self, tool: &str, subject: &str, read_only: bool) -> Decision {
        let mut decision = Decision::Allow;
        let mut matched = false;

        for check in &self.checks {
            match check.check(tool, subject) {
                Some(Decision::Deny(_)) => return Decision::Deny("denied".into()),
                Some(Decision::Ask) => { decision = Decision::Ask; matched = true; }
                Some(Decision::Allow) => { matched = true; /* Allow 不升级，保留当前 */ }
                None => { /* 不匹配，跳过 */ }
            }
        }

        // 没有任何 Check 匹配 → 用 fallback
        if !matched {
            if read_only {
                return Decision::Allow;
            }
            return self.writer_fallback.clone();
        }

        decision
    }

    /// 对多 subject 工具调用进行评估（如 move_file 的 src+dst）。
    ///
    /// 每个 subject 独立评估，然后用 `combine` 合并：
    /// 任一 Deny → 整体 Deny；任一 Ask → 整体 Ask（除非有 Deny）。
    pub fn evaluate_subjects(&self, tool: &str, subjects: &[String], read_only: bool) -> Decision {
        if subjects.is_empty() {
            return self.evaluate(tool, "", read_only);
        }

        let mut overall = Decision::Allow;
        for subject in subjects {
            overall = overall.combine(self.evaluate(tool, subject, read_only));
        }
        overall
    }

    /// 对复合 bash 命令进行 per-segment 评估。
    ///
    /// 将命令按 &&、||、|、;、\\n 拆解为独立 segment，
    /// 每个 segment 独立评估，然后合并：
    /// 任一 Deny → 整体 Deny；任一 Ask → 整体 Ask（除非有 Deny）。
    ///
    /// 如果命令不含操作符或分解失败，回退到 `evaluate`。
    pub fn evaluate_compound(&self, tool: &str, command: &str, read_only: bool) -> Decision {
        if tool != "bash" {
            return self.evaluate(tool, command, read_only);
        }

        let segments = match bash_decompose::decompose(command) {
            Some(segs) if segs.len() <= 1 => return self.evaluate(tool, command, read_only),
            Some(segs) => segs,
            None => return self.evaluate(tool, command, read_only),
        };

        let mut overall = Decision::Allow;
        for seg in &segments {
            overall = overall.combine(self.evaluate(tool, seg, read_only));
            if matches!(overall, Decision::Deny(_)) {
                return Decision::Deny("denied".into());
            }
        }
        overall
    }
}

// ── BlockedReason ────────────────────────────────────────────────────────────

/// 工具调用被阻止的原因。
#[derive(Debug, Clone)]
pub enum BlockedReason {
    /// 被静态规则拒绝。
    Denied(String),
    /// 需要用户批准（非交互模式下退化为 Allow）。
    NeedsApproval(String),
    /// Guard 内部错误。
    Error(String),
}

impl std::fmt::Display for BlockedReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockedReason::Denied(msg) => write!(f, "denied by permission policy — {}", msg),
            BlockedReason::NeedsApproval(msg) => write!(f, "needs approval — {}", msg),
            BlockedReason::Error(msg) => write!(f, "permission error — {}", msg),
        }
    }
}

impl BlockedReason {
    /// 是否被拒绝（Denied 类别）。
    pub fn is_denied(&self) -> bool {
        matches!(self, BlockedReason::Denied(_))
    }

    
}

// ── Approver trait ───────────────────────────────────────────────────────────

/// 审批上下文——Gate 传递给 Approver 的信息。
#[derive(Debug, Clone)]
pub struct ApprovalContext {
    /// 工具名称。
    pub tool: String,
    /// 工具调用的 subject（命令字符串、文件路径等）。
    pub subject: String,
    /// 完整的 JSON 参数。
    pub args: serde_json::Value,
}

/// 交互式审批器。
///
/// 在 TUI/CLI 场景中，`Approver` 向用户弹窗询问是否允许。
/// 非交互模式（`None`）下 `Ask` 退化为 `Allow`。
#[async_trait::async_trait]
pub trait Approver: Send + Sync {
    /// 询问用户是否允许此次调用。
    ///
    /// 返回 `true` = 允许，`false` = 拒绝。
    async fn approve(&self, ctx: &ApprovalContext) -> Result<bool, String>;
}

// ── Gate ─────────────────────────────────────────────────────────────────────

/// 权限门禁——Agent 在 execute 前调用的最终入口。
///
/// 组合 Policy（规则引擎）和可选的 Approver（交互审批），
/// 决定一次工具调用是否可以执行。
pub struct Gate {
    policy: Policy,
    approver: Option<Box<dyn Approver>>,
}

impl Gate {
    /// 创建一个新 Gate，不含交互式审批器。
    /// 非交互模式下，Ask 退化为 Allow。
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            approver: None,
        }
    }

    /// 设置交互式审批器。
    pub fn with_approver(mut self, approver: Box<dyn Approver>) -> Self {
        self.approver = Some(approver);
        self
    }

    /// 返回底层 Policy 的引用。
    pub fn policy(&self) -> &Policy {
        &self.policy
    }

    /// 检查一次工具调用是否允许。
    ///
    /// # 参数
    /// - `tool`: 工具名（如 `"edit_file"`, `"bash"`）
    /// - `args`: 工具调用的 JSON 参数
    /// - `read_only`: 工具是否被标记为只读
    ///
    /// # 返回
    /// - `Ok(())` — 允许执行
    /// - `Err(BlockedReason)` — 被拒绝或需审批
    pub fn check(&self, tool: &str, args: &serde_json::Value, read_only: bool) -> Result<(), BlockedReason> {
        // bash 工具：如果命令本身是只读的，覆盖 read_only 标记
        let effective_read_only = if tool == "bash" && !read_only {
            let subject = extract_subject(args);
            if !subject.is_empty() && super::checks::is_read_only_bash(&subject) {
                true
            } else {
                read_only
            }
        } else {
            read_only
        };

        let subjects = extract_subjects(args);
        let decision = if subjects.len() > 1 {
            self.policy.evaluate_subjects(tool, &subjects, effective_read_only)
        } else {
            let subject = subjects.first().map(|s| s.as_str()).unwrap_or("");
            // bash 复合命令：per-segment 评估
            if tool == "bash" && !subject.is_empty() {
                self.policy.evaluate_compound(tool, subject, effective_read_only)
            } else {
                self.policy.evaluate(tool, subject, effective_read_only)
            }
        };

        match decision {
            Decision::Allow => Ok(()),
            Decision::Ask => {
                if self.approver.is_none() {
                    // 非交互模式：Ask 退化为 Allow
                    return Ok(());
                }

                let subject = subjects.first().map(|s| s.as_str()).unwrap_or("");
                let ctx = ApprovalContext {
                    tool: tool.to_string(),
                    subject: subject.to_string(),
                    args: args.clone(),
                };

                // 这里只在同步上下文中调用——实际场景中 Gate 应该是 async
                // 目前返回 NeedsApproval 让上层处理
                Err(BlockedReason::NeedsApproval(
                    format!("tool {} ({}) needs approval", tool, subject)
                ))
            }
            Decision::Deny(_) => {
                let subject = subjects.first().map(|s| s.as_str()).unwrap_or("");
                Err(BlockedReason::Denied(
                    format!("tool {} ({}) is denied by policy", tool, subject)
                ))
            }
        }
    }

    /// 异步版本的 check——当有 Approver 时使用。
    pub async fn check_async(&self, tool: &str, args: &serde_json::Value, read_only: bool) -> Result<(), BlockedReason> {
        // bash 工具：如果命令本身是只读的，覆盖 read_only 标记
        let effective_read_only = if tool == "bash" && !read_only {
            let subject = extract_subject(args);
            if !subject.is_empty() && super::checks::is_read_only_bash(&subject) {
                true
            } else {
                read_only
            }
        } else {
            read_only
        };

        let subjects = extract_subjects(args);
        let decision = if subjects.len() > 1 {
            self.policy.evaluate_subjects(tool, &subjects, effective_read_only)
        } else {
            let subject = subjects.first().map(|s| s.as_str()).unwrap_or("");
            // bash 复合命令：per-segment 评估
            if tool == "bash" && !subject.is_empty() {
                self.policy.evaluate_compound(tool, subject, effective_read_only)
            } else {
                self.policy.evaluate(tool, subject, effective_read_only)
            }
        };

        match decision {
            Decision::Allow => Ok(()),
            Decision::Ask => {
                let approver = match &self.approver {
                    Some(a) => a,
                    None => return Ok(()), // 非交互：退化
                };

                let subject = subjects.first().map(|s| s.as_str()).unwrap_or("");
                let ctx = ApprovalContext {
                    tool: tool.to_string(),
                    subject: subject.to_string(),
                    args: args.clone(),
                };

                match approver.approve(&ctx).await {
                    Ok(true) => Ok(()),
                    Ok(false) => Err(BlockedReason::Denied(
                        "user declined this tool call".into()
                    )),
                    Err(e) => Err(BlockedReason::Error(e)),
                }
            }
            Decision::Deny(_) => {
                let subject = subjects.first().map(|s| s.as_str()).unwrap_or("");
                Err(BlockedReason::Denied(
                    format!("tool {} ({}) is denied by policy", tool, subject)
                ))
            }
        }
    }
    /// 检查一次工具调用，考虑 AgentMode 升降级。
    ///
    /// 与 `check()` 相同，但根据 agent_mode 调整 Ask/Allow：
    /// - Unrestrained：Ask → Allow
    /// - Cautious：非只读工具的 Allow → 返回 NeedsApproval
    pub fn check_with_mode(
        &self,
        tool: &str,
        args: &serde_json::Value,
        read_only: bool,
        agent_mode: AgentMode,
    ) -> Result<(), BlockedReason> {
        match agent_mode {
            AgentMode::Unrestrained => {
                // 放飞自我：先评估，Ask 退化为 Allow，Deny 仍生效
                let result = self.check(tool, args, read_only);
                match result {
                    Err(BlockedReason::NeedsApproval(_)) => Ok(()),
                    other => other,
                }
            }
            AgentMode::Cautious => {
                // 谨慎：先评估，如果是 Allow 但工具非只读 → 转为 NeedsApproval
                let result = self.check(tool, args, read_only);
                match result {
                    Ok(()) if !read_only => Err(BlockedReason::NeedsApproval(
                        format!("cautious mode: {} needs approval", tool)
                    )),
                    other => other,
                }
            }
            _ => self.check(tool, args, read_only),
        }
    }

    /// 异步版本的 check_with_mode。
    pub async fn check_with_mode_async(
        &self,
        tool: &str,
        args: &serde_json::Value,
        read_only: bool,
        agent_mode: AgentMode,
    ) -> Result<(), BlockedReason> {
        match agent_mode {
            AgentMode::Unrestrained => {
                let result = self.check_async(tool, args, read_only).await;
                match result {
                    Err(BlockedReason::NeedsApproval(_)) => Ok(()),
                    other => other,
                }
            }
            AgentMode::Cautious => {
                let result = self.check_async(tool, args, read_only).await;
                match result {
                    Ok(()) if !read_only => Err(BlockedReason::NeedsApproval(
                        format!("cautious mode: {} needs approval", tool)
                    )),
                    other => other,
                }
            }
            _ => self.check_async(tool, args, read_only).await,
        }
    }
}

impl std::fmt::Debug for Gate {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gate")
            .field("writer_fallback", &self.policy.writer_fallback)
            .field("has_approver", &self.approver.is_some())
            .finish()
    }
}

// ── 工具权限检查助手 ─────────────────────────────────────────────────────────

/// PermissionChecked trait：工具携带自己的权限检查列表，在 execute 入口调用。
///
/// 工具 struct 实现此 trait，`check_permission()` 自动在 execute 最前面拦截。
pub trait PermissionChecked {
    fn permission_checks(&self) -> &[Box<dyn Check>];

    /// Agent 的全局门控模式是否对此工具有效。默认 true。
    /// 如果为 false，该工具的 check_permission 不受 agent_mode 影响。
    fn agent_mode_enabled(&self) -> bool { true }

    /// 执行所有权限检查。
    ///
    /// `agent_mode` 控制 Ask 的升降级：
    /// - Cautious：无 Check 拒绝但也不是只读时 → Ask
    /// - Unrestrained：Ask → Allow（从不询问）
    fn check_permission(&self, tool: &str, args: &serde_json::Value, agent_mode: AgentMode) -> Decision {
        let subjects = extract_subjects(args);
        let mut decision = Decision::Allow;
        let mut matched = false;

        for check in self.permission_checks() {
            for subject in &subjects {
                match check.check(tool, subject) {
                    Some(Decision::Deny(_)) => {
                        return Decision::Deny(format!(
                            "denied by {}: {} is not allowed",
                            check.name(),
                            subject
                        ));
                    }
                    Some(Decision::Ask) => { decision = Decision::Ask; matched = true; }
                    Some(Decision::Allow) => { matched = true; }
                    None => {}
                }
            }
        }

        match agent_mode {
            AgentMode::Unrestrained => {
                if decision == Decision::Ask {
                    decision = Decision::Allow;
                }
            }
            AgentMode::Cautious => {
                if !matched && decision == Decision::Allow {
                    let read_only_tools = [
                        "read_file", "ls", "glob", "grep", "web_fetch",
                        "code_index", "bash_output", "read_only_bash",
                    ];
                    if !read_only_tools.contains(&tool) {
                        decision = Decision::Ask;
                    }
                }
            }
            AgentMode::Ask | AgentMode::Auto => {}
        }

        if decision == Decision::Ask {
            return Decision::Ask;
        }
        Decision::Allow
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::checks::*;

    // ── Policy ──

    #[test]
    fn policy_allows_readonly_without_checks() {
        let p = Policy::new(Decision::Ask, vec![]);
        assert_eq!(p.evaluate("read_file", "whatever", true), Decision::Allow);
    }

    #[test]
    fn policy_writer_fallback_ask() {
        let p = Policy::new(Decision::Ask, vec![]);
        assert_eq!(p.evaluate("edit_file", "whatever", false), Decision::Ask);
    }

    #[test]
    fn policy_writer_fallback_deny() {
        let p = Policy::new(Decision::Deny("denied".into()), vec![]);
        assert_eq!(p.evaluate("edit_file", "whatever", false), Decision::Deny("denied".into()));
    }

    #[test]
    fn policy_deny_overrides_ask() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(MockCheck::ask("bash", "safe")),
            Box::new(MockCheck::deny("bash", "danger")),
        ]);
        // 应该返回 Deny（优先级最高）
        assert_eq!(p.evaluate("bash", "danger", false), Decision::Deny("denied".into()));
    }

    #[test]
    fn policy_ask_overrides_allow() {
        let p = Policy::new(Decision::Allow, vec![
            Box::new(MockCheck::allow("bash", "safe")),
            Box::new(MockCheck::ask("bash", "suspicious")),
        ]);
        assert_eq!(p.evaluate("bash", "suspicious", false), Decision::Ask);
    }

    #[test]
    fn policy_no_match_uses_fallback() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(MockCheck::deny("edit_file", "/etc/*")), // 不匹配 bash
        ]);
        assert_eq!(p.evaluate("bash", "ls", false), Decision::Ask);
    }

    #[test]
    fn policy_with_real_checks() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DenySystemPaths),
            Box::new(ForcePushGuard),
        ]);
        assert_eq!(p.evaluate("edit_file", "/etc/passwd", false), Decision::Deny("denied".into()));
        assert_eq!(p.evaluate("bash", "git push --force", false), Decision::Ask);
    }

    #[test]
    fn policy_subjects_combine() {
        let p = Policy::new(Decision::Allow, vec![
            Box::new(MockCheck::deny("move_file", "/etc/*")),
        ]);
        assert_eq!(
            p.evaluate_subjects("move_file", &["/home/a.txt".into(), "/etc/b.txt".into()], false),
            Decision::Deny("denied".into())
        );
    }

    #[test]
    fn policy_subjects_ask_wins_over_allow() {
        let p = Policy::new(Decision::Allow, vec![
            Box::new(MockCheck::ask("move_file", "/secret/*")),
        ]);
        assert_eq!(
            p.evaluate_subjects("move_file", &["/home/a.txt".into(), "/secret/b.txt".into()], false),
            Decision::Ask
        );
    }

    // ── Gate ──

    #[test]
    fn gate_allows_readonly() {
        let p = Policy::new(Decision::Ask, vec![]);
        let g = Gate::new(p);
        let args = serde_json::json!({"path": "Cargo.toml"});
        assert!(g.check("read_file", &args, true).is_ok());
    }

    #[test]
    fn gate_denies_system_path() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DenySystemPaths),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"file_path": "/etc/passwd"});
        let result = g.check("edit_file", &args, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_denied());
    }

    #[test]
    fn gate_ask_falls_back_to_allow_noninteractive() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(MockCheck::ask("bash", "*")),
        ]);
        let g = Gate::new(p); // no approver
        let args = serde_json::json!({"command": "rm -rf /"});
        assert!(g.check("bash", &args, false).is_ok());
    }

    #[test]
    fn gate_bash_readonly_auto_detected() {
        let p = Policy::new(Decision::Deny("denied".into()), vec![]);
        let g = Gate::new(p);
        // echo 被识别为只读 → 即使 writer_fallback=Deny，也应该 Allow
        let args = serde_json::json!({"command": "echo hello"});
        assert!(g.check("bash", &args, false).is_ok());
    }

    #[test]
    fn gate_bash_writer_still_denied() {
        let p = Policy::new(Decision::Deny("denied".into()), vec![]);
        let g = Gate::new(p);
        // rm 不是只读 → writer_fallback=Deny 生效
        let args = serde_json::json!({"command": "rm -rf /tmp/foo"});
        let result = g.check("bash", &args, false);
        assert!(result.is_err());
    }

    
    
    // ── 辅助 Mock ──

    struct MockCheck {
        tool: &'static str,
        pattern: &'static str,
        decision: Decision,
    }

    impl MockCheck {
        fn allow(tool: &'static str, pattern: &'static str) -> Self {
            Self { tool, pattern, decision: Decision::Allow }
        }
        fn ask(tool: &'static str, pattern: &'static str) -> Self {
            Self { tool, pattern, decision: Decision::Ask }
        }
        fn deny(tool: &'static str, pattern: &'static str) -> Self {
            Self { tool, pattern, decision: Decision::Deny("denied".into()) }
        }
    }

    impl Check for MockCheck {
        fn name(&self) -> &'static str {
            "mock_check"
        }

        fn check(&self, tool: &str, subject: &str) -> Option<Decision> {
            if tool != self.tool {
                return None;
            }
            if crate::permission::match_glob(self.pattern, subject) {
                Some(self.decision.clone())
            } else {
                None
            }
        }
    }

    // ── 集成测试 ──

    #[test]
    fn permission_checked_with_deny_system() {
        struct WithSystemCheck {
            checks: Vec<Box<dyn Check>>,
        }
        impl PermissionChecked for WithSystemCheck {
            fn permission_checks(&self) -> &[Box<dyn Check>] { &self.checks }
        }
        let t = WithSystemCheck {
            checks: vec![Box::new(DenySystemPaths)],
        };
        let args = serde_json::json!({"file_path": "/etc/passwd"});
        let result = t.check_permission("edit_file", &args, AgentMode::Ask);
        assert!(!matches!(result, Decision::Allow));
        assert!(matches!(result, Decision::Deny(msg) if msg.contains("denied")));

        let args = serde_json::json!({"file_path": "/home/user/project/main.rs"});
        assert!(matches!(t.check_permission("edit_file", &args, AgentMode::Ask), Decision::Allow));
    }

    #[test]
    fn policy_integration_mixed_checks() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DenySystemPaths),
            Box::new(ForcePushGuard),
            Box::new(ReadOnlyBashClassifier),
        ]);
        assert_eq!(p.evaluate("edit_file", "/etc/passwd", false), Decision::Deny("denied".into()));
        assert_eq!(p.evaluate("bash", "git push --force", false), Decision::Ask);
        assert_eq!(p.evaluate("bash", "ls -la", false), Decision::Allow);
        assert_eq!(p.evaluate("write_file", "/ok/path.txt", false), Decision::Ask);
        // DenySystemPaths 对任何工具的 /etc 路径都匹配
        assert_eq!(p.evaluate("read_file", "/etc/passwd", true), Decision::Deny("denied".into()));
        // 安全路径的 read_file → Allow
        assert_eq!(p.evaluate("read_file", "/home/user/file.txt", true), Decision::Allow);
    }

    #[test]
    fn gate_noninteractive_ask_fallback() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ForcePushGuard),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"command": "git push --force"});
        assert!(g.check("bash", &args, false).is_ok());
    }

    #[test]
    fn decision_combine_integration() {
        let p = Policy::new(Decision::Allow, vec![
            Box::new(MockCheck::deny("move_file", "/etc/*")),
            Box::new(MockCheck::ask("move_file", "/secret/*")),
        ]);
        assert_eq!(
            p.evaluate_subjects("move_file", &["/home/a.txt".into(), "/etc/passwd".into()], false),
            Decision::Deny("denied".into())
        );
        assert_eq!(
            p.evaluate_subjects("move_file", &["/home/a.txt".into(), "/secret/data".into()], false),
            Decision::Ask
        );
        assert_eq!(
            p.evaluate_subjects("move_file", &["/home/a.txt".into(), "/home/b.txt".into()], false),
            Decision::Allow
        );
    }

    #[test]
    fn danger_and_deny_independent() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DangerousPatternDetector),
        ]);
        assert_eq!(p.evaluate("bash", "rm -rf /", false), Decision::Ask);

        let p2 = Policy::new(Decision::Ask, vec![
            Box::new(DenySystemPaths),
            Box::new(DangerousPatternDetector),
        ]);
        assert_eq!(p2.evaluate("edit_file", "/etc/passwd", false), Decision::Deny("denied".into()));
        assert_eq!(p2.evaluate("bash", "rm -rf /", false), Decision::Ask);
    }

    // ── AgentMode 测试 ──

    #[test]
    fn permission_checked_unrestrained_asks_become_allow() {
        struct WithAskCheck {
            checks: Vec<Box<dyn Check>>,
        }
        impl PermissionChecked for WithAskCheck {
            fn permission_checks(&self) -> &[Box<dyn Check>] { &self.checks }
        }
        let t = WithAskCheck {
            checks: vec![Box::new(MockCheck::ask("bash", "*"))],
        };
        let args = serde_json::json!({"command": "rm -rf /"});
        // Ask mode → 需要询问
        assert!(!matches!(t.check_permission("bash", &args, AgentMode::Ask), Decision::Allow));
        // Unrestrained mode → 放行（Ask 降级为 Allow）
        assert!(matches!(t.check_permission("bash", &args, AgentMode::Unrestrained), Decision::Allow));
    }

    #[test]
    fn permission_checked_cautious_mode_asks_writers() {
        struct NoChecks;
        impl PermissionChecked for NoChecks {
            fn permission_checks(&self) -> &[Box<dyn Check>] { &[] }
        }
        let t = NoChecks;
        let args = serde_json::json!({"path": "/tmp/x.txt"});
        // Cautious mode + 写工具 → no check matched → 触发询问
        assert!(!matches!(t.check_permission("edit_file", &args, AgentMode::Cautious), Decision::Allow));
        // Cautious mode + 只读工具 → 不触发
        assert!(matches!(t.check_permission("read_file", &args, AgentMode::Cautious), Decision::Allow));
    }

    #[test]
    fn gate_check_with_mode_cautious() {
        let p = Policy::new(Decision::Allow, vec![]);
        let g = Gate::new(p);
        let args = serde_json::json!({"path": "/tmp/x.txt"});

        // Cautious + write tool → NeedsApproval
        let result = g.check_with_mode("edit_file", &args, false, AgentMode::Cautious);
        assert!(matches!(result, Err(BlockedReason::NeedsApproval(_))));

        // Cautious + read tool → Ok
        let result = g.check_with_mode("read_file", &args, true, AgentMode::Cautious);
        assert!(result.is_ok());
    }

    #[test]
    fn gate_check_with_mode_unrestrained() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ForcePushGuard),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"command": "git push --force"});


        // Unrestrained → Allow (Ask 降级)
        let result = g.check_with_mode("bash", &args, false, AgentMode::Unrestrained);
        assert!(result.is_ok());
    }

    #[test]
    fn gate_check_with_mode_deny_still_works() {
        let p = Policy::new(Decision::Allow, vec![
            Box::new(DenySystemPaths),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"file_path": "/etc/passwd"});

        // Unrestrained 不阻止 Deny
        let result = g.check_with_mode("edit_file", &args, false, AgentMode::Unrestrained);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_denied());
    }

    // ── 场景测试：SafeDirectoryGuard ───────────────────────────────────────
    //
    // 这是一个完整的 Check 场景测试，模拟一个"安全目录守卫"规则：
    // - 拒绝写入系统临时目录（/tmp/、/var/tmp/、/private/tmp/）
    // - 允许写入项目目录（/home/user/project/）
    // - 只读操作不受影响
    // - 通过 PermissionChecked trait 模拟真实工具使用方式

    /// 拒绝写入临时目录的 Check。
    ///
    /// 作用于所有文件变异工具（write_file、edit_file 等）。
    /// 检查 subject 是否以临时目录前缀开头。
    struct SafeDirectoryGuard;

    const TEMP_PATH_PREFIXES: &[&str] = &[
        "/tmp/",
        "/var/tmp/",
        "/private/tmp/",
    ];

    impl Check for SafeDirectoryGuard {
        fn name(&self) -> &'static str {
            "safe_directory_guard"
        }

        fn check(&self, tool: &str, subject: &str) -> Option<Decision> {
            if subject.is_empty() {
                return None;
            }
            for prefix in TEMP_PATH_PREFIXES {
                if subject.starts_with(prefix) {
                    return Some(Decision::Deny("denied".into()));
                }
            }
            // 对已知的可写项目目录直接放行（减少不必要的 Ask）
            if subject.starts_with("/home/user/project/") {
                return Some(Decision::Allow);
            }
            None
        }
    }
    

    

    /// 场景测试：Policy + SafeDirectoryGuard 组合
    #[test]
    fn policy_with_safe_directory_guard() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(SafeDirectoryGuard),
            Box::new(DenySystemPaths),
        ]);

        // 临时路径 → Deny
        assert_eq!(p.evaluate("write_file", "/tmp/x.txt", false), Decision::Deny("denied".into()));
        // 系统路径 → Deny（来自 DenySystemPaths）
        assert_eq!(p.evaluate("edit_file", "/etc/config", false), Decision::Deny("denied".into()));
        // 项目路径 → Allow（来自 SafeDirectoryGuard）
        assert_eq!(p.evaluate("edit_file", "/home/user/project/main.rs", false), Decision::Allow);
        // 未知路径 → fallback Ask
        assert_eq!(p.evaluate("write_file", "/home/user/other.txt", false), Decision::Ask);
        // 只读操作 → Allow
        assert_eq!(p.evaluate("read_file", "/tmp/x.txt", true), Decision::Allow);
    }

    /// 场景测试：Gate + SafeDirectoryGuard 完整链路
    #[test]
    fn gate_with_safe_directory_guard_full_flow() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(SafeDirectoryGuard),
        ]);
        let g = Gate::new(p);

        // 1. 写文件到 /tmp/ → Deny
        let args = serde_json::json!({"file_path": "/tmp/foo.txt"});
        let result = g.check("edit_file", &args, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_denied());

        // 2. 写文件到项目路径 → Allow（SafeDirectoryGuard 明确放行）
        let args = serde_json::json!({"file_path": "/home/user/project/src/lib.rs"});
        assert!(g.check("edit_file", &args, false).is_ok());

        // 3. 写文件到未知路径 → Ask → 非交互模式退化为 Allow
        let args = serde_json::json!({"file_path": "/home/user/Downloads/notes.txt"});
        assert!(g.check("edit_file", &args, false).is_ok());

        // 4. 只读文件 → 即使路径在 /tmp/ 也 Allow（read_only 覆盖）
        let args = serde_json::json!({"file_path": "/tmp/readme.md"});
        assert!(g.check("read_file", &args, true).is_ok());

        // 5. bash 命令 → SafeDirectoryGuard 不适用（非文件工具）→ fallback Ask → 非交互退化为 Allow
        let args = serde_json::json!({"command": "ls /tmp"});
        assert!(g.check("bash", &args, false).is_ok());

        // 6. 写文件到 /tmp/ + Cautious 模式 → Deny（Deny 优先于 Cautious）
        let args = serde_json::json!({"file_path": "/tmp/foo.txt"});
        let result = g.check_with_mode("edit_file", &args, false, AgentMode::Cautious);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_denied());

        // 7. 写文件到未知路径 + Cautious 模式 → NeedsApproval（Cautious 提升 Ask）
        let args = serde_json::json!({"file_path": "/some/random/path.txt"});
        let result = g.check_with_mode("write_file", &args, false, AgentMode::Cautious);
        assert!(matches!(result, Err(BlockedReason::NeedsApproval(_))));
    }

    /// 场景测试：PermissionChecked + SafeDirectoryGuard（模拟真实工具用法）
    #[test]
    fn permission_checked_with_safe_directory_guard() {
        struct Editor {
            checks: Vec<Box<dyn Check>>,
        }
        impl PermissionChecked for Editor {
            fn permission_checks(&self) -> &[Box<dyn Check>] { &self.checks }
        }

        let editor = Editor {
            checks: vec![
                Box::new(SafeDirectoryGuard),
                Box::new(DenySystemPaths),
            ],
        };

        // 写入 /tmp/ → 被 DenySystemPaths 或 SafeDirectoryGuard 拒绝
        let args = serde_json::json!({"file_path": "/tmp/test.txt"});
        let result = editor.check_permission("edit_file", &args, AgentMode::Ask);
        assert!(!matches!(result, Decision::Allow));
        assert!(matches!(result, Decision::Deny(msg) if msg.contains("denied")));

        // 写入项目路径 → Allow（无返回）
        let args = serde_json::json!({"file_path": "/home/user/project/main.rs"});
        assert!(matches!(editor.check_permission("edit_file", &args, AgentMode::Ask), Decision::Allow));

        // 写入系统路径 /etc/ → 被 DenySystemPaths 拒绝
        let args = serde_json::json!({"file_path": "/etc/nginx.conf"});
        let result = editor.check_permission("edit_file", &args, AgentMode::Ask);
        assert!(!matches!(result, Decision::Allow));
        assert!(matches!(result, Decision::Deny(msg) if msg.contains("denied")));

        // 写入未知路径 → 无 Check 匹配 → check_permission 不阻止
        // （Policy/Gate 层有 writer_fallback=Ask 兜底，但 check_permission 本身没有）
        let args = serde_json::json!({"file_path": "/srv/app/config.json"});
        assert!(matches!(editor.check_permission("edit_file", &args, AgentMode::Ask), Decision::Allow));

        // Cautious 模式下，无 Check 匹配的写操作会被 Ask
        let args = serde_json::json!({"file_path": "/srv/app/config.json"});
        assert!(!matches!(editor.check_permission("edit_file", &args, AgentMode::Cautious), Decision::Allow));

        // Unrestrained 模式下 Ask 退化为 Allow（包括 Cautious 导致的 Ask）
        let args = serde_json::json!({"file_path": "/srv/app/config.json"});
        assert!(matches!(editor.check_permission("edit_file", &args, AgentMode::Unrestrained), Decision::Allow));
    }

    /// 场景测试：多 subject（move_file） 与 SafeDirectoryGuard
    #[test]
    fn safe_directory_guard_with_move_file() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(SafeDirectoryGuard),
        ]);

        // 从项目移到 /tmp/ → /tmp/ 被 Deny
        assert_eq!(
            p.evaluate_subjects("move_file", &[
                "/home/user/project/a.txt".into(),
                "/tmp/a.txt".into(),
            ], false),
            Decision::Deny("denied".into())
        );

        // 从 /tmp/ 移到项目 → /tmp/ 被 Deny（任一 subject 触发即 Deny）
        assert_eq!(
            p.evaluate_subjects("move_file", &[
                "/tmp/a.txt".into(),
                "/home/user/project/a.txt".into(),
            ], false),
            Decision::Deny("denied".into())
        );

        // 项目内移动 → Allow（两个 subject 都匹配项目路径）
        assert_eq!(
            p.evaluate_subjects("move_file", &[
                "/home/user/project/a.txt".into(),
                "/home/user/project/b.txt".into(),
            ], false),
            Decision::Allow
        );

        // 从项目移到其他未知目录 → Ask（未知路径 fallback）
        assert_eq!(
            p.evaluate_subjects("move_file", &[
                "/home/user/project/a.txt".into(),
                "/home/user/Downloads/a.txt".into(),
            ], false),
            Decision::Ask
        );
    }

    // ── Compound bash 分解集成测试 ────────────────────────────────────────
    //
    // 测试 Policy::evaluate_compound 和 Gate::check 对复合 bash 命令
    // 的 per-segment 评估。

    #[test]
    fn compound_simple_command_falls_back() {
        // 不含操作符的简单命令 → 回退到 evaluate
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
        ]);
        assert_eq!(p.evaluate_compound("bash", "ls -la", false), Decision::Allow);
        assert_eq!(p.evaluate_compound("bash", "rm -rf /", false), Decision::Ask);
    }

    #[test]
    fn compound_non_bash_falls_back() {
        // 非 bash 工具 → 回退到 evaluate
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DenySystemPaths),
        ]);
        assert_eq!(
            p.evaluate_compound("edit_file", "/tmp/x.txt", false),
            p.evaluate("edit_file", "/tmp/x.txt", false)
        );
    }

    #[test]
    fn compound_safe_chain_all_allowed() {
        // 全部安全的 chain → Allow
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
            Box::new(ForcePushGuard),
            Box::new(DangerousPatternDetector),
        ]);
        assert_eq!(
            p.evaluate_compound("bash", "ls -la && echo hello && git status", false),
            Decision::Allow
        );
    }

    #[test]
    fn compound_readonly_then_force_push() {
        // 前半段只读，后半段 force push → Ask（ForcePushGuard 触发）
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
            Box::new(ForcePushGuard),
            Box::new(DangerousPatternDetector),
        ]);
        assert_eq!(
            p.evaluate_compound("bash", "git status && git push --force origin main", false),
            Decision::Ask
        );
    }

    #[test]
    fn compound_readonly_then_dangerous() {
        // 前半段只读，后半段危险命令 → Ask（DangerousPatternDetector 触发）
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
            Box::new(DangerousPatternDetector),
        ]);
        assert_eq!(
            p.evaluate_compound("bash", "ls -la && rm -rf /tmp/foo", false),
            Decision::Ask
        );
    }

    #[test]
    fn compound_force_push_denied() {
        // 任何一段触发 Deny → 整体 Deny
        struct DenyForcePush;
        impl Check for DenyForcePush {
            fn name(&self) -> &'static str { "deny_force_push" }
            fn check(&self, tool: &str, subject: &str) -> Option<Decision> {
                if tool == "bash" && subject.contains("git push --force") {
                    Some(Decision::Deny("denied".into()))
                } else { None }
            }
        }
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DenyForcePush),
            Box::new(ReadOnlyBashClassifier),
        ]);
        assert_eq!(
            p.evaluate_compound("bash", "git status && git push --force origin main", false),
            Decision::Deny("denied".into())
        );
    }

    #[test]
    fn compound_triple_pipe() {
        // 多段管道——所有命令都是已知只读命令
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
        ]);
        assert_eq!(
            // cat, head, wc 都是已知只读命令
            p.evaluate_compound("bash", "cat file.txt | head -5 | wc -l", false),
            Decision::Allow
        );
    }

    #[test]
    fn compound_mixed_operators() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
            Box::new(DangerousPatternDetector),
        ]);
        assert_eq!(
            p.evaluate_compound("bash", "cd src && cargo build; echo done", false),
            Decision::Ask
        );
    }

    #[test]
    fn compound_with_quoted_operators_not_split() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
        ]);
        assert_eq!(
            p.evaluate_compound("bash", "echo 'hello && world'", false),
            Decision::Allow
        );
    }

    // ── Gate 集成测试：compound bash 通过 Gate::check ──

    #[test]
    fn gate_compound_readonly_then_force_push() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
            Box::new(ForcePushGuard),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"command": "git status && git push --force origin main"});
        assert!(g.check("bash", &args, false).is_ok());
    }

    #[test]
    fn gate_compound_readonly_only() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(ReadOnlyBashClassifier),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"command": "ls -la && echo hello && git status"});
        assert!(g.check("bash", &args, false).is_ok());
    }

    #[test]
    fn gate_compound_partial_deny() {
        struct DenyRmRf;
        impl Check for DenyRmRf {
            fn name(&self) -> &'static str { "deny_rm_rf" }
            fn check(&self, tool: &str, subject: &str) -> Option<Decision> {
                if tool == "bash" && subject.trim() == "rm -rf /" {
                    Some(Decision::Deny("denied".into()))
                } else { None }
            }
        }
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DenyRmRf),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"command": "echo hello && rm -rf /"});
        let result = g.check("bash", &args, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_denied());
    }

    #[test]
    fn gate_compound_not_bash_uses_evaluate() {
        let p = Policy::new(Decision::Ask, vec![
            Box::new(DenySystemPaths),
        ]);
        let g = Gate::new(p);
        let args = serde_json::json!({"file_path": "/etc/passwd"});
        let result = g.check("edit_file", &args, false);
        assert!(result.is_err());
        assert!(result.unwrap_err().is_denied());
    }
}
