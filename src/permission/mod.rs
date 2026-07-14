// permission —— 工具调用的权限控制层。
//
// 架构概述
// =========
// 采用组合模式（composable checks）：每个 `Check` 是最小的校验单元，
// 工具在构造时通过 `Vec<Box<dyn Check>>` 拼装自己的权限策略。
//
// 决策优先级（不可覆盖）：Deny > Ask > Allow > fallback
//
// ┌────────────┐      ┌──────────────┐      ┌──────────┐
// │  工具      │ ──→  │ check_permission │ ──→ │ Decision │
// │ (checks:   │      │ (遍历所有 check) │      └──────────┘
// │  Vec<..>)  │      └──────────────┘
// └────────────┘
//
// 增减一个检查 = 在工具的 new() 中增减一行 `Box::new(SomeCheck)`。

use std::fmt;

pub mod bash_decompose;
pub mod checks;
pub mod gate;

// ── Decision ─────────────────────────────────────────────────────────────────

/// 权限决策结果。
///
/// 优先级（从高到低）：Deny > Ask > Allow
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 允许执行。
    Allow,
    /// 需要询问用户（或委托给 Guardian 子代理）。
    Ask,
    /// 拒绝执行，模型不应重试。
    Deny,
}

impl Decision {
    /// 将两个决策按优先级合并：Deny > Ask > Allow
    /// 用于同时有多个 subject 时（如 move_file 的 src + dst）。
    pub fn combine(self, other: Decision) -> Decision {
        match (self, other) {
            (Decision::Deny, _) | (_, Decision::Deny) => Decision::Deny,
            (Decision::Ask, _) | (_, Decision::Ask) => Decision::Ask,
            _ => Decision::Allow,
        }
    }
}

impl fmt::Display for Decision {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Decision::Allow => write!(f, "allow"),
            Decision::Ask => write!(f, "ask"),
            Decision::Deny => write!(f, "deny"),
        }
    }
}

/// 将字符串解析为 Decision。未知/空输入默认为 Ask（保守的写操作 fallback）。
pub fn parse_decision(s: &str) -> Decision {
    match s.trim().to_lowercase().as_str() {
        "allow" => Decision::Allow,
        "deny" => Decision::Deny,
        _ => Decision::Ask,
    }
}

// ── Check trait ──────────────────────────────────────────────────────────────

/// 一次独立的权限检查。
///
/// 每个 `Check` 代表一条可复用的规则。工具在构造时拼装自己的检查列表：
///
/// ```ignore
/// let checks: Vec<Box<dyn Check>> = vec![
///     Box::new(DenySystemPaths),
///     Box::new(MaxFileSize::new(10 * 1024 * 1024)),
/// ];
/// ```
///
/// - 返回 `Some(decision)` 表示匹配，以此决策为准（按优先级合并）
/// - 返回 `None` 表示不适用，跳过
pub trait Check: Send + Sync {
    /// 检查的名称，用于日志/调试。
    fn name(&self) -> &'static str;

    /// 对一次工具调用执行检查。
    ///
    /// - `tool`: 工具名（如 `"edit_file"`, `"bash"`, `"write_file"`）
    /// - `subject`: 调用的主体（文件路径、命令字符串、URL 等）
    ///
    /// 返回 `None` 表示此规则不适用（跳过），
    /// 返回 `Some(decision)` 表示匹配。
    fn check(&self, tool: &str, subject: &str) -> Option<Decision>;
}

// ── Subject 提取 ─────────────────────────────────────────────────────────────

/// JSON 参数中可能包含"主体"的 key，按优先级排列。
const SUBJECT_KEYS: &[&str] = &[
    "command",
    "file_path",
    "path",
    "source_path",
    "destination_path",
    "pattern",
];

/// 从工具调用的 JSON args 中提取主要 subject。
///
/// 返回第一个匹配的 key 的值；如果没有任何已知 key，返回空字符串。
/// 这允许规则只匹配 bare `ToolName`（无 subject 限制）。
pub fn extract_subject(args: &serde_json::Value) -> String {
    extract_subjects(args).into_iter().next().unwrap_or_default()
}

/// 提取所有 subject（如 move_file 有 source_path 和 destination_path）。
///
/// 用于双路径保护：两端都必须通过检查，调用方才被允许。
pub fn extract_subjects(args: &serde_json::Value) -> Vec<String> {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return vec![],
    };

    // move_file 特殊处理：source_path + destination_path
    let src = obj.get("source_path").and_then(|v| v.as_str()).filter(|s| !s.is_empty());
    let dst = obj.get("destination_path").and_then(|v| v.as_str()).filter(|s| !s.is_empty());

    if let (Some(src), Some(dst)) = (src, dst) {
        if src == dst {
            return vec![src.to_string()];
        }
        return vec![src.to_string(), dst.to_string()];
    }

    for key in SUBJECT_KEYS {
        if let Some(val) = obj.get(*key).and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
            return vec![val.to_string()];
        }
    }

    vec![]
}

// ── Glob 匹配 ────────────────────────────────────────────────────────────────

/// 通配符匹配：`*` 匹配任意字符序列（含 `/`），`?` 匹配单个字符。
/// 与 `std::path::Path::matches` 不同，`*` 不受目录分隔符限制，
/// 这符合命令行和路径前缀的直觉（`rm -rf*` 应匹配 `rm -rf /`）。
///
/// 线性时间 + 回溯，按字节匹配。
pub fn match_glob(pattern: &str, name: &str) -> bool {
    let pb = pattern.as_bytes();
    let nb = name.as_bytes();
    let (mut pi, mut ni) = (0, 0);
    let (mut star_pi, mut star_ni) = (usize::MAX, 0);

    while ni < nb.len() {
        match () {
            _ if pi < pb.len() && (pb[pi] == b'?' || pb[pi] == nb[ni]) => {
                pi += 1;
                ni += 1;
            }
            _ if pi < pb.len() && pb[pi] == b'*' => {
                star_pi = pi;
                star_ni = ni;
                pi += 1;
            }
            _ if star_pi != usize::MAX => {
                pi = star_pi + 1;
                star_ni += 1;
                ni = star_ni;
            }
            _ => return false,
        }
    }

    // 跳过末尾的 *
    while pi < pb.len() && pb[pi] == b'*' {
        pi += 1;
    }

    pi == pb.len()
}

// ── 规则解析 ─────────────────────────────────────────────────────────────────

/// 解析一条规则字符串为 `(tool, subject_pattern)`。
///
/// 格式：
/// - `"ToolName"` — 匹配所有对该工具的调用
/// - `"ToolName(subject_glob)"` — 匹配特定 subject
///
/// 返回 `None` 表示格式错误（空的工具名）。
pub fn parse_rule(s: &str) -> Option<(String, String)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    // 尝试 ToolName(subject) 格式
    if let Some(paren) = s.find('(') {
        if s.ends_with(')') {
            let tool = s[..paren].trim();
            let subject = s[paren + 1..s.len() - 1].trim();
            if !tool.is_empty() {
                return Some((tool.to_string(), subject.to_string()));
            }
        }
    }

    // 裸工具名
    Some((s.to_string(), String::new()))
}

/// 检查一条规则字符串是否匹配给定的 (tool, subject)。
pub fn rule_matches(rule: &str, tool: &str, subject: &str) -> bool {
    let Some((rule_tool, rule_subject)) = parse_rule(rule) else {
        return false;
    };

    // 工具名必须匹配（大小写敏感）
    if rule_tool != tool {
        return false;
    }

    // 无 subject 限制 → 匹配所有调用
    if rule_subject.is_empty() {
        return true;
    }

    // 有 subject 限制 → subject 也必须匹配
    if subject.is_empty() {
        return false;
    }

    match_glob(&rule_subject, subject)
}

// ── 工具分类 ─────────────────────────────────────────────────────────────────
/// 判断一个工具是否属于"文件变异"类（会修改工作区文件）。
///
/// 这类工具共享文件写入侧的权限规则（如拒绝写入系统路径）。
pub fn is_file_mutation_tool(tool: &str) -> bool {
    matches!(
        tool,
        "write_file"
            | "edit_file"
            | "multi_edit"
            | "move_file"
            | "notebook_edit"
            | "delete_range"
            | "delete_symbol"
    )
}

// ── 便利函数 ─────────────────────────────────────────────────────────────────

/// 检查一个 bash 命令是否是危险命令（仅用于 UI 警告，不替代规则执行）。
pub fn danger_warning(subject: &str) -> Option<&'static str> {
    let dangerous: &[(&str, &str)] = &[
        ("rm -rf*", "recursive delete"),
        ("rm -fr*", "recursive delete"),
        ("git push*--force*", "force push"),
        ("git push*-f*", "force push"),
        ("chmod 777*", "world-writable"),
        ("sudo *", "superuser"),
        ("dd if=*", "raw device write"),
        ("> /dev/*", "device overwrite"),
    ];

    let s = subject.trim();
    for (pattern, label) in dangerous {
        if match_glob(pattern, s) {
            return Some(label);
        }
    }
    None
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Decision ──

    #[test]
    fn decision_combine_deny_wins() {
        assert_eq!(Decision::Deny, Decision::Allow.combine(Decision::Deny));
        assert_eq!(Decision::Deny, Decision::Deny.combine(Decision::Allow));
        assert_eq!(Decision::Deny, Decision::Ask.combine(Decision::Deny));
    }

    #[test]
    fn decision_combine_ask_second() {
        assert_eq!(Decision::Ask, Decision::Allow.combine(Decision::Ask));
        assert_eq!(Decision::Ask, Decision::Ask.combine(Decision::Allow));
    }

    #[test]
    fn decision_combine_allow_allow() {
        assert_eq!(Decision::Allow, Decision::Allow.combine(Decision::Allow));
    }

    #[test]
    fn parse_decision_defaults_to_ask() {
        assert_eq!(Decision::Ask, parse_decision(""));
        assert_eq!(Decision::Ask, parse_decision("unknown"));
    }

    #[test]
    fn parse_decision_cases() {
        assert_eq!(Decision::Allow, parse_decision("allow"));
        assert_eq!(Decision::Deny, parse_decision("deny"));
        assert_eq!(Decision::Ask, parse_decision("ask"));
    }

    // ── Glob 匹配 ──

    #[test]
    fn glob_exact() {
        assert!(match_glob("hello", "hello"));
        assert!(!match_glob("hello", "world"));
    }

    #[test]
    fn glob_star() {
        assert!(match_glob("rm -rf*", "rm -rf /"));
        assert!(match_glob("rm -rf*", "rm -rf"));
        assert!(match_glob("rm -rf*", "rm -rf /etc/passwd"));
        assert!(!match_glob("rm -rf*", "rm -r"));
    }

    #[test]
    fn glob_question() {
        assert!(match_glob("file.?", "file.c"));
        assert!(match_glob("file.?", "file.h"));
        assert!(!match_glob("file.?", "file.rs"));
    }

    #[test]
    fn glob_prefix_with_colon() {
        assert!(match_glob("git push:*", "git push:origin main"));
        assert!(match_glob("git push:*", "git push:--force"));
    }

    #[test]
    fn glob_trailing_star() {
        assert!(match_glob("git *", "git push"));
        assert!(match_glob("git *", "git status"));
        assert!(!match_glob("git *", "gits"));
    }

    // ── 规则解析 ──

    #[test]
    fn parse_rule_bare_tool() {
        let (tool, subject) = parse_rule("Bash").unwrap();
        assert_eq!(tool, "Bash");
        assert!(subject.is_empty());
    }

    #[test]
    fn parse_rule_with_subject() {
        let (tool, subject) = parse_rule("Bash(git push:*)").unwrap();
        assert_eq!(tool, "Bash");
        assert_eq!(subject, "git push:*");
    }

    #[test]
    fn parse_rule_empty_returns_none() {
        assert!(parse_rule("").is_none());
        assert!(parse_rule("  ").is_none());
    }

    #[test]
    fn parse_rule_nested_parens() {
        // 括号内内容原样保留
        let (tool, subject) = parse_rule("Edit(/path/to/file)").unwrap();
        assert_eq!(tool, "Edit");
        assert_eq!(subject, "/path/to/file");
    }

    // ── 规则匹配 ──

    #[test]
    fn rule_matches_bare_tool() {
        assert!(rule_matches("Bash", "Bash", "any command"));
        assert!(!rule_matches("Bash", "Edit", "any"));
    }

    #[test]
    fn rule_matches_with_subject() {
        assert!(rule_matches("Bash(rm -rf*)", "Bash", "rm -rf /"));
        assert!(!rule_matches("Bash(rm -rf*)", "Bash", "git push"));
    }

    #[test]
    fn rule_matches_empty_subject_requires_subject() {
        // 规则有 subject 限制，但调用无 subject → 不匹配
        assert!(!rule_matches("Bash(rm*)", "Bash", ""));
    }

    // ── Subject 提取 ──

    #[test]
    fn extract_subject_from_command() {
        let args = serde_json::json!({"command": "ls -la"});
        assert_eq!(extract_subject(&args), "ls -la");
    }

    #[test]
    fn extract_subject_from_path() {
        let args = serde_json::json!({"path": "/tmp/file.txt"});
        assert_eq!(extract_subject(&args), "/tmp/file.txt");
    }

    #[test]
    fn extract_subjects_move_file() {
        let args = serde_json::json!({
            "source_path": "/a/b.txt",
            "destination_path": "/c/d.txt"
        });
        let subjects = extract_subjects(&args);
        assert_eq!(subjects, vec!["/a/b.txt", "/c/d.txt"]);
    }

    #[test]
    fn extract_subjects_move_file_same() {
        let args = serde_json::json!({
            "source_path": "/a/b.txt",
            "destination_path": "/a/b.txt"
        });
        let subjects = extract_subjects(&args);
        assert_eq!(subjects, vec!["/a/b.txt"]); // 去重
    }

    #[test]
    fn extract_subject_empty_args() {
        let args = serde_json::json!({});
        assert!(extract_subject(&args).is_empty());
    }

    // ── 工具分类 ──

    #[test]
    fn is_file_mutation_positive() {
        assert!(is_file_mutation_tool("write_file"));
        assert!(is_file_mutation_tool("edit_file"));
        assert!(is_file_mutation_tool("multi_edit"));
    }

    #[test]
    fn is_file_mutation_negative() {
        assert!(!is_file_mutation_tool("bash"));
        assert!(!is_file_mutation_tool("grep"));
        assert!(!is_file_mutation_tool("read_file"));
    }

    // ── 危险警告 ──

    #[test]
    fn danger_warning_rm_rf() {
        assert_eq!(danger_warning("rm -rf /"), Some("recursive delete"));
        assert_eq!(danger_warning("rm -rf --no-preserve-root /etc"), Some("recursive delete"));
    }

    #[test]
    fn danger_warning_safe() {
        assert!(danger_warning("ls -la").is_none());
        assert!(danger_warning("echo hello").is_none());
    }

    #[test]
    fn danger_warning_force_push() {
        assert_eq!(danger_warning("git push --force origin main"), Some("force push"));
    }
}
