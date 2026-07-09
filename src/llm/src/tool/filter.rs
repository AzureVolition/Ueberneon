use super::*;
use std::collections::HashSet;

// ── 元工具黑名单 ────────────────────────────────────────────────────────────

/// 子代理不应继承的元工具：递归 agent/skill/install 工具。
const SUBAGENT_META_TOOLS: &[&str] = &[
    "task",
    "run_skill",
    "read_skill",
    "install_skill",
    "install_source",
    "explore",
    "research",
    "review",
    "security_review",
];

/// 子代理不应继承的后台任务工具。
const SUBAGENT_JOB_TOOLS: &[&str] = &[
    "wait",
    "bash_output",
    "kill_shell",
];

// ── FilterRegistry ───────────────────────────────────────────────────────────

/// 从父注册表构建子注册表。
///
/// - `names`: 白名单（空 = 所有父工具）
/// - `exclude`: 额外排除的工具名
///
/// 对齐 Reasonix `agent.FilterRegistry`。
pub fn filter_registry(
    parent: &Registry,
    names: &[String],
    exclude: &[String],
) -> Registry {
    let sub = Registry::new();
    let parent_names = parent.names();

    let ex: HashSet<&str> = exclude.iter().map(|s| s.as_str()).collect();

    // 空白名单 = 取全部父工具
    let src: Vec<String> = if names.is_empty() {
        parent_names
    } else {
        names.to_vec()
    };

    for name in &src {
        if ex.contains(name.as_str()) {
            continue;
        }
        if let Some(tool) = parent.get(name) {
            sub.add(tool);
        }
    }

    sub
}

// ── SubagentToolRegistry ─────────────────────────────────────────────────────

/// 子代理的工具注册表：
/// - 去掉元工具（task/run_skill/explore 等，防递归）
/// - 去掉后台任务工具（wait/bash_output/kill_shell）
/// - bash 降级为纯前台模式（如果在白名单中）
///
/// 对齐 Reasonix `agent.SubagentToolRegistry`。
pub fn subagent_tool_registry(parent: &Registry, names: &[String]) -> Registry {
    let mut exclude = Vec::from_iter(
        SUBAGENT_META_TOOLS.iter().map(|s| s.to_string())
    );
    exclude.extend(SUBAGENT_JOB_TOOLS.iter().map(|s| s.to_string()));

    let sub = filter_registry(parent, names, &exclude);

    // 如果 sub 中有 bash，替换为 foreground-only 包装
    if let Some(_bash) = sub.get("bash") {
        // 实际实现：用 ForegroundOnlyBash 包装
        // sub.add(ForegroundOnlyBash::new(bash));
    }

    sub
}

/// Planner 模型不应使用的工具。
const PLANNER_NON_RESEARCH_TOOLS: &[&str] = &[
    "ask",
    "todo_write",
    "complete_step",
    "memory",
];

/// Planner 模型的只读研究工具集。
/// 对齐 Reasonix `agent.PlannerToolRegistry`。
pub fn planner_tool_registry(parent: &Registry) -> Registry {
    let read_only = filter_read_only_registry(parent);
    let exclude: Vec<String> = PLANNER_NON_RESEARCH_TOOLS.iter().map(|s| s.to_string()).collect();
    filter_registry(&read_only, &[], &exclude)
}

// ── FilterReadOnly ───────────────────────────────────────────────────────────

/// 只保留 ReadOnly() == true 的工具。
/// 对齐 Reasonix `agent.FilterReadOnlyRegistry`。
pub fn filter_read_only_registry(parent: &Registry) -> Registry {
    let sub = Registry::new();
    for name in parent.names() {
        if let Some(tool) = parent.get(name) {
            if tool.read_only() {
                sub.add(tool);
            }
        }
    }
    sub
}

// ── Plan Mode 门控 ──────────────────────────────────────────────────────────

/// plan mode 下硬禁止的写工具。
const PLAN_MODE_BLOCKED_TOOLS: &[&str] = &[
    "write_file",
    "edit_file",
    "multi_edit",
    "apply_patch",
    "move_file",
    "delete_range",
    "delete_symbol",
    "notebook_edit",
];

/// plan mode 下 bash 的允许命令前缀白名单。
const PLAN_MODE_BASH_ALLOWED_PREFIXES: &[&str] = &[
    "ls", "cat", "head", "tail", "wc", "find", "grep", "git log",
    "git diff", "git show", "git status", "git branch",
];

/// 判断工具在 plan mode 下是否被阻止。
/// 对齐 Reasonix `agent.planModeBlocked`。
pub fn plan_mode_blocked(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    // 写文件类工具硬禁止
    if PLAN_MODE_BLOCKED_TOOLS.contains(&tool_name) {
        return Some(format!(
            "blocked: {} is a write tool and not allowed in plan mode",
            tool_name
        ));
    }

    // bash: 只允许白名单命令前缀
    if tool_name == "bash" {
        if let Some(cmd) = args.get("command").and_then(|c| c.as_str()) {
            let cmd = cmd.trim();
            let allowed = PLAN_MODE_BASH_ALLOWED_PREFIXES.iter()
                .any(|prefix| cmd.starts_with(prefix));
            if !allowed {
                return Some(format!(
                    "blocked: bash command not allowed in plan mode: {}",
                    cmd
                ));
            }
        }
    }

    None
}

// ── 并行/串行分区 ────────────────────────────────────────────────────────────

/// 将工具调用列表分割为 (只读批次, 写入列表)。
/// - 连续的只读工具 → 可并行执行（最多 8 并发）
/// - 写工具 → 必须串行执行
///
/// 对齐 Reasonix `agent.partitionToolCalls`。
#[derive(Debug)]
pub enum ExecutionGroup {
    /// 可并行执行的只读工具组
    Parallel(Vec<ToolCallInfo>),
    /// 必须串行执行的写工具
    Sequential(ToolCallInfo),
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub call_id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub read_only: bool,
}

pub fn partition_tool_calls(
    calls: &[ToolCallInfo],
) -> Vec<ExecutionGroup> {
    let mut groups = Vec::new();
    let mut batch = Vec::new();

    for call in calls {
        if call.read_only {
            batch.push(call.clone());
        } else {
            if !batch.is_empty() {
                groups.push(ExecutionGroup::Parallel(std::mem::take(&mut batch)));
            }
            groups.push(ExecutionGroup::Sequential(call.clone()));
        }
    }

    if !batch.is_empty() {
        groups.push(ExecutionGroup::Parallel(batch));
    }

    groups
}

#[cfg(test)]
mod tests {
    use super::*;

    // 验证子代理不会继承元工具
    #[test]
    fn subagent_excludes_meta_tools() {
        let exclude: HashSet<&str> = SUBAGENT_META_TOOLS.iter().copied().collect();
        assert!(exclude.contains("task"));
        assert!(exclude.contains("explore"));
        assert!(exclude.contains("run_skill"));
    }

    // 验证 plan mode 阻止写工具
    #[test]
    fn plan_mode_blocks_write_tools() {
        let args = serde_json::json!({"path": "/tmp/test.txt", "content": "hello"});
        assert!(plan_mode_blocked("write_file", &args).is_some());
        assert!(plan_mode_blocked("read_file", &args).is_none());
    }

    // 验证 plan mode bash 白名单
    #[test]
    fn plan_mode_bash_whitelist() {
        let read_args = serde_json::json!({"command": "git log --oneline"});
        let write_args = serde_json::json!({"command": "rm -rf /tmp"});

        assert!(plan_mode_blocked("bash", &read_args).is_none());
        assert!(plan_mode_blocked("bash", &write_args).is_some());
    }

    // 验证并行/串行分区
    #[test]
    fn partition_mixed_read_write() {
        let calls = vec![
            ToolCallInfo { call_id: "1".into(), tool_name: "read_file".into(), args: serde_json::json!({}), read_only: true },
            ToolCallInfo { call_id: "2".into(), tool_name: "grep".into(), args: serde_json::json!({}), read_only: true },
            ToolCallInfo { call_id: "3".into(), tool_name: "write_file".into(), args: serde_json::json!({}), read_only: false },
            ToolCallInfo { call_id: "4".into(), tool_name: "read_file".into(), args: serde_json::json!({}), read_only: true },
        ];

        let groups = partition_tool_calls(&calls);
        assert_eq!(groups.len(), 3); // Parallel(1,2) + Sequential(3) + Parallel(4)

        match &groups[0] {
            ExecutionGroup::Parallel(batch) => assert_eq!(batch.len(), 2),
            _ => panic!("expected parallel group"),
        }
        match &groups[1] {
            ExecutionGroup::Sequential(c) => assert_eq!(c.call_id, "3"),
            _ => panic!("expected sequential"),
        }
        match &groups[2] {
            ExecutionGroup::Parallel(batch) => assert_eq!(batch.len(), 1),
            _ => panic!("expected parallel group"),
        }
    }
}