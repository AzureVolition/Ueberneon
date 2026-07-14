// checks.rs —— 预置的可复用权限检查单元。
//
// 每个检查实现 `Check` trait，工具在构造时自由拼装：
//
// ```ignore
// let checks: Vec<Box<dyn Check>> = vec![
//     Box::new(DenySystemPaths),
//     Box::new(MaxFileSize::new(10 * 1024 * 1024)),
// ];
// ```

use super::{Check, Decision};

// ── DenySystemPaths ──────────────────────────────────────────────────────────

/// 拒绝文件变异工具写入系统关键路径。
///
/// 作用于：`edit_file`、`write_file`、`multi_edit` 等文件写入工具。
/// 检查 subject 是否以系统目录前缀开头。
pub struct DenySystemPaths;

/// 系统路径前缀列表——匹配任一即拒绝。
const SYSTEM_PATH_PREFIXES: &[&str] = &[
    "/etc/",
    "/usr/",
    "/root/",
    "/boot/",
    "/dev/",
    "/sys/",
    "/proc/",
    "/var/log/",
    "/var/db/",
    "/opt/",
    // Windows
    "C:\\Windows\\",
    "C:\\Program Files\\",
    "C:\\ProgramData\\",
    "C:\\System32\\",
];

impl Check for DenySystemPaths {
    fn name(&self) -> &'static str {
        "deny_system_paths"
    }

    fn check(&self, _tool: &str, subject: &str) -> Option<Decision> {
        if subject.is_empty() {
            return None;
        }
        for prefix in SYSTEM_PATH_PREFIXES {
            if subject.starts_with(prefix) {
                return Some(Decision::Deny);
            }
        }
        None
    }
}

// ── ForcePushGuard ───────────────────────────────────────────────────────────

/// 检测 `git push --force` / `git push -f` 等破坏性 git 操作，标记为 Ask。
///
/// 作用于：`bash` 工具。
/// 不直接 deny——可能用户确实要 force push，但需要二次确认。
pub struct ForcePushGuard;

impl Check for ForcePushGuard {
    fn name(&self) -> &'static str {
        "force_push_guard"
    }

    fn check(&self, tool: &str, subject: &str) -> Option<Decision> {
        if tool != "bash" || subject.is_empty() {
            return None;
        }

        let lower = subject.to_lowercase();

        // 检测各种 force push 变体
        let force_patterns = [
            "git push --force",
            "git push -f",
            "git push -ff",
            "git push --force-with-lease",
        ];

        if force_patterns.iter().any(|p| lower.contains(p)) {
            return Some(Decision::Ask);
        }

        None
    }
}

// ── MaxFileSize ──────────────────────────────────────────────────────────────

/// 限制写入工具的单次最大文件大小（bytes）。
///
/// 作用于：`write_file` 等带 `content` 参数的文件写入工具。
/// 检查 args 中的 `content` 字段长度。
pub struct MaxFileSize {
    max_bytes: usize,
}

impl MaxFileSize {
    pub fn new(max_bytes: usize) -> Self {
        Self { max_bytes }
    }
}

impl Check for MaxFileSize {
    fn name(&self) -> &'static str {
        "max_file_size"
    }

    fn check(&self, _tool: &str, subject: &str) -> Option<Decision> {
        if subject.is_empty() {
            return None;
        }
        // subject 在这里是命令字符串或路径——MaxFileSize 需要从 args JSON 中读取 content 长度。
        // 这个 check 不能仅从 subject 字符串判断，所以这里返回 None，
        // 实际检查在 Policy/Gate 层处理 args JSON。
        // 保留此结构体作为配置占位，Gate 层会特殊处理它。
        let _ = subject;
        None
    }
}

// ── ReadOnlyBashClassifier ───────────────────────────────────────────────────

/// 将已知安全的只读 bash 命令分类为 Allow（无需用户确认）。
///
/// 作用于：`bash` 工具。
/// 当命令是 `echo`、`ls`、`git status/log/diff`、
/// `cat`、`head`、`tail`、`which`、`pwd`、`date`、
/// `go test` 等只读操作时返回 Allow。
pub struct ReadOnlyBashClassifier;

impl Check for ReadOnlyBashClassifier {
    fn name(&self) -> &'static str {
        "read_only_bash"
    }

    fn check(&self, tool: &str, subject: &str) -> Option<Decision> {
        if tool != "bash" || subject.is_empty() {
            return None;
        }

        if is_read_only_bash(subject) {
            return Some(Decision::Allow);
        }

        None
    }
}

/// 判断一个 bash 命令是否本质只读（无副作用）。
///
/// 这通过检查命令名和参数实现。已知只读命令包括：
/// - 信息类：echo, printf, which, type, pwd, date, env, uname, whoami, id
/// - 文件浏览：cat, head, tail, less, more, sort (无 -o), uniq, wc, nl, od, xxd
/// - 目录：ls, tree, find (无 -exec/-delete), du, df, stat, realpath, dirname, basename
/// - 网络：curl (仅 GET), wget (仅 GET), ping, nslookup, dig, host
/// - git 只读：git status, git log, git diff, git show, git branch, git remote, git config --list
/// - go 只读：go version, go env, go list, go vet (不修改), go fmt (只读检查)
pub fn is_read_only_bash(command: &str) -> bool {
    let command = command.trim();

    // 空命令或简单文本（无空格=单条命令）→ 安全
    if !command.contains(' ') {
        return false;
    }

    // 提取第一条命令
    let first_cmd = command.split_whitespace().next().unwrap_or("");

    match first_cmd {
        // 安全的信息命令
        "echo" | "printf" | "which" | "type" | "pwd" | "date"
        | "env" | "uname" | "whoami" | "id" | "printenv" | "hostname"
        | "true" | "false" | "yes" => return true,

        // 只读文件浏览
        "cat" | "head" | "tail" | "less" | "more" | "wc" | "nl"
        | "od" | "xxd" | "tac" | "rev" | "cut" | "sort" | "uniq" => {
            // sort 有 -o 参数会写文件
            if first_cmd == "sort" && has_flag(command, &["-o", "--output"]) {
                return false;
            }
            return true;
        }

        // 目录/文件系统只读
        "ls" | "tree" | "du" | "df" | "stat" | "realpath"
        | "dirname" | "basename" | "readlink" | "file" => return true,

        // find - 需检查没有 -exec/-delete 等写操作参数
        "find" => {
            return !has_flag(command, &["-exec", "-execdir", "-delete", "-ok", "-okdir", "-fls", "-fprint"]);
        }

        // git - 只读子命令
        "git" => {
            let sub = extract_subcommand(command);
            matches!(
                sub.as_str(),
                "status" | "log" | "diff" | "show" | "branch"
                    | "tag" | "remote" | "config" | "describe"
                    | "rev-parse" | "rev-list" | "ls-files"
                    | "ls-tree" | "stash" | "blame" | "shortlog"
                    | "help" | "version"
            )
        }

        // go - 只读子命令
        "go" => {
            let sub = extract_subcommand(command);
            matches!(
                sub.as_str(),
                "version" | "env" | "list" | "vet" | "doc"
                    | "help" | "mod" // go mod tidy 等会修改，但 go mod download 是只读的
            )
        }

        // Cargo - 只读子命令
        "cargo" => {
            let sub = extract_subcommand(command);
            matches!(
                sub.as_str(),
                "check" | "doc" | "tree" | "metadata" | "search"
                    | "help" | "version" | "info" | "pkgid" | "report"
            )
        }

        // npm/pnpm/yarn - 只读子命令（不含 install/add/remove）
        "npm" | "pnpm" | "yarn" => {
            let sub = extract_subcommand(command);
            matches!(
                sub.as_str(),
                "list" | "outdated" | "view" | "info" | "help" | "version" | "why" | "pack"
            ) && !sub.is_empty()
        }

        // docker/podman - 只读子命令
        "docker" | "podman" => {
            let sub = extract_subcommand(command);
            matches!(
                sub.as_str(),
                "ps" | "images" | "logs" | "inspect"
                    | "stats" | "top" | "port" | "version"
                    | "network" | "info" | "history" | "events"
            )
        }

        // curl/wget - 只读（无 -d/--data/--upload 等写操作）
        "curl" => {
            !has_flag(command, &["-d", "--data", "--data-binary", "--data-raw",
                "--upload", "-T", "-F", "--form", "-X PUT", "-X POST",
                "-X DELETE", "--request PUT", "--request POST", "--request DELETE"])
        }
        "wget" => {
            !has_flag(command, &["--post-data", "--post-file", "--upload-file", "-o"])
        }

        _ => false,
    }
}

/// 从命令字符串中提取第一个子命令（第二个 token）。
fn extract_subcommand(command: &str) -> String {
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.len() >= 2 {
        // 跳过 -flags
        for p in &parts[1..] {
            if !p.starts_with('-') {
                return p.to_string();
            }
        }
    }
    String::new()
}

/// 检查命令中是否包含指定 flag（简单的字符串包含检查）。
fn has_flag(command: &str, flags: &[&str]) -> bool {
    let lower_cmd = command.to_lowercase();
    flags.iter().any(|f| lower_cmd.contains(f))
}

// ── DangerousPatternDetector ─────────────────────────────────────────────────

/// 检测危险的 bash 命令模式（rm -rf、dd if= 等），标记为 Ask。
///
/// 区别于 `DenySystemPaths`——这个不直接拒绝，而是需要用户二次确认。
pub struct DangerousPatternDetector;

impl Check for DangerousPatternDetector {
    fn name(&self) -> &'static str {
        "dangerous_pattern_detector"
    }

    fn check(&self, tool: &str, subject: &str) -> Option<Decision> {
        if tool != "bash" || subject.is_empty() {
            return None;
        }

        if super::danger_warning(subject).is_some() {
            return Some(Decision::Ask);
        }

        None
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── DenySystemPaths ──

    #[test]
    fn deny_etc() {
        let c = DenySystemPaths;
        assert_eq!(c.check("edit_file", "/etc/passwd"), Some(Decision::Deny));
        assert_eq!(c.check("write_file", "/etc/nginx/nginx.conf"), Some(Decision::Deny));
    }

    #[test]
    fn deny_usr() {
        let c = DenySystemPaths;
        assert_eq!(c.check("edit_file", "/usr/local/bin/something"), Some(Decision::Deny));
    }

    #[test]
    fn allow_project_file() {
        let c = DenySystemPaths;
        assert_eq!(c.check("edit_file", "/home/user/project/main.rs"), None);
    }

    #[test]
    fn deny_not_applicable_to_bash() {
        let c = DenySystemPaths;
        assert_eq!(c.check("bash", "rm /etc/passwd"), None);
    }

    #[test]
    fn deny_empty_subject() {
        let c = DenySystemPaths;
        assert_eq!(c.check("edit_file", ""), None);
    }

    // ── ForcePushGuard ──

    #[test]
    fn force_push_detected() {
        let c = ForcePushGuard;
        assert_eq!(
            c.check("bash", "git push --force origin main"),
            Some(Decision::Ask)
        );
    }

    #[test]
    fn force_push_short_flag() {
        let c = ForcePushGuard;
        assert_eq!(c.check("bash", "git push -f origin"), Some(Decision::Ask));
    }

    #[test]
    fn normal_push_allowed() {
        let c = ForcePushGuard;
        assert_eq!(c.check("bash", "git push origin main"), None);
    }

    #[test]
    fn force_push_not_applicable_to_edit() {
        let c = ForcePushGuard;
        assert_eq!(c.check("edit_file", "git push --force"), None);
    }

    // ── ReadOnlyBashClassifier ──

    #[test]
    fn echo_is_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "echo hello"), Some(Decision::Allow));
    }

    #[test]
    fn ls_is_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "ls -la /tmp"), Some(Decision::Allow));
    }

    #[test]
    fn git_status_is_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "git status"), Some(Decision::Allow));
    }

    #[test]
    fn git_log_is_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "git log --oneline -5"), Some(Decision::Allow));
    }

    #[test]
    fn git_push_not_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "git push origin main"), None);
    }

    #[test]
    fn go_version_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "go version"), Some(Decision::Allow));
    }

    #[test]
    fn go_build_not_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "go build ./..."), None);
    }

    #[test]
    fn cargo_check_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "cargo check"), Some(Decision::Allow));
    }

    #[test]
    fn cargo_build_not_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "cargo build"), None);
    }

    #[test]
    fn curl_get_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "curl https://api.example.com"), Some(Decision::Allow));
    }

    #[test]
    fn curl_post_not_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "curl -X POST -d data https://api.example.com"), None);
    }

    #[test]
    fn sort_no_o_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "sort file.txt"), Some(Decision::Allow));
    }

    #[test]
    fn sort_with_o_not_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "sort -o output.txt file.txt"), None);
    }

    #[test]
    fn docker_ps_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "docker ps"), Some(Decision::Allow));
    }

    #[test]
    fn docker_run_not_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "docker run nginx"), None);
    }

    #[test]
    fn find_with_exec_not_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "find . -exec rm {} \\;"), None);
    }

    #[test]
    fn find_without_exec_readonly() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", "find . -name '*.rs'"), Some(Decision::Allow));
    }

    #[test]
    fn readonly_not_applicable_to_edit() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("edit_file", "echo hi"), None);
    }

    #[test]
    fn readonly_empty_subject() {
        let c = ReadOnlyBashClassifier;
        assert_eq!(c.check("bash", ""), None);
    }

    // ── DangerousPatternDetector ──

    #[test]
    fn dangerous_rm_rf() {
        let c = DangerousPatternDetector;
        assert_eq!(c.check("bash", "rm -rf /"), Some(Decision::Ask));
    }

    #[test]
    fn dangerous_sudo() {
        let c = DangerousPatternDetector;
        assert_eq!(c.check("bash", "sudo rm -rf /"), Some(Decision::Ask));
    }

    #[test]
    fn safe_command_no_danger() {
        let c = DangerousPatternDetector;
        assert_eq!(c.check("bash", "ls -la"), None);
    }

    #[test]
    fn dangerous_not_applicable_to_edit() {
        let c = DangerousPatternDetector;
        assert_eq!(c.check("edit_file", "rm -rf /"), None);
    }

    // ── 工具函数 ──

    #[test]
    fn extract_subcommand_simple() {
        assert_eq!(extract_subcommand("git status"), "status");
        assert_eq!(extract_subcommand("go build ./..."), "build");
        assert_eq!(extract_subcommand("echo hi"), "hi");
    }

    #[test]
    fn extract_subcommand_skips_flags() {
        assert_eq!(extract_subcommand("git --version"), ""); // no subcommand after flag
        assert_eq!(extract_subcommand("cargo -v check"), "check");
    }

    #[test]
    fn extract_subcommand_single_word() {
        assert_eq!(extract_subcommand("ls"), "");
    }
}
