// read_only_bash.rs —— 只读 Bash 工具（仅允许安全的只读命令）。
//
// 与 Bash 不同：read_only=true，不支持后台模式，始终强制执行
// plan-mode 白名单检查。用于 subagent / explore 等只读场景。
//
// 底层复用 ProcessRunner / Shell / EnvBuilder / SandboxSpec。

use std::path::PathBuf;
use std::time::Duration;

use crate::agent::{Tool, AgentHandler, ToolContext, ToolResult};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use crate::agent::{AgentMode, ActionMode, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;

use super::common::env::EnvBuilder;
use super::common::process::{ProcessOutput, ProcessRunner};
use super::common::shell::Shell;
use crate::tools::sandbox::SandboxSpec;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// read_only_bash —— 只读安全的 shell 命令执行。
///
/// 只允许 plan-mode 白名单中的只读命令（ls、cat、head、tail、wc、
/// find、grep、git log/diff/show/status/branch）。
/// 不支持后台执行。不会修改文件系统。
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(schema = r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute (read-only commands only)"},"description":{"type":"string","description":"Optional description of what the command does"}},"required":["command"]}"#)]
pub struct ReadOnlyBash {
    /// 工作目录。
    work_dir: PathBuf,
    /// 超时时间。
    timeout: Duration,
    /// 沙箱配置（可选）。
    sandbox: Option<SandboxSpec>,
    /// 缓存的 shell 探测结果。
    shell: std::sync::OnceLock<Shell>,
}

impl ReadOnlyBash {
    pub fn new(work_dir: PathBuf, timeout: Duration, sandbox: Option<SandboxSpec>) -> Self {
        Self {
            work_dir,
            timeout,
            sandbox,
            shell: std::sync::OnceLock::new(),
        }
    }

    /// 获取（或初始化）探测到的 shell。
    fn get_shell(&self) -> Shell {
        *self.shell.get_or_init(Shell::detect)
    }

    /// 构建子进程环境变量：继承 + PATH 合并 + secrets 过滤。
    fn build_env(&self) -> std::collections::HashMap<String, String> {
        let mut builder = EnvBuilder::inherit();
        builder.merge_login_path();
        builder.filter_secrets();
        builder.build()
    }

    /// plan mode 下 bash 的允许命令前缀白名单。
    const PLAN_MODE_ALLOWED_PREFIXES: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "find", "grep", "git log",
        "git diff", "git show", "git status", "git branch",
    ];

    /// 检查命令是否在白名单中。
    fn check_read_only(command: &str) -> Option<String> {
        let cmd = command.trim();
        let allowed = Self::PLAN_MODE_ALLOWED_PREFIXES
            .iter()
            .any(|prefix| cmd.starts_with(prefix));
        if !allowed {
            Some(format!(
                "blocked: read_only_bash: command '{}' is not in the read-only whitelist",
                cmd
            ))
        } else {
            None
        }
    }

    /// 将 ProcessOutput 转为 ToolResult。
    fn build_tool_result(output: ProcessOutput) -> Result<ToolResult, String> {
        if output.exit_code != 0 {
            let detail = if output.combined.is_empty() {
                "(no output)".to_string()
            } else {
                let preview: String = output.combined.lines().take(5).collect::<Vec<_>>().join("\n");
                format!("output:\n{preview}")
            };
            let msg = if output.timed_out {
                format!(
                    "command timed out (exit code {})\n{detail}",
                    output.exit_code
                )
            } else {
                format!(
                    "command exited with code {}\n{detail}",
                    output.exit_code
                )
            };
            return Err(msg);
        }

        Ok(ToolResult { output: output.combined, truncated: output.truncated })
    }
}

#[async_trait::async_trait]
impl Tool for ReadOnlyBash {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        // 1. 解析参数
        let command = match args.get("command").and_then(|v| v.as_str()) {
            Some(c) if !c.trim().is_empty() => c,
            _ => return Err("read_only_bash: missing required argument 'command'".into()),
        };

        let timeout = args
            .get("timeout")
            .and_then(|v| v.as_u64())
            .map(Duration::from_secs)
            .unwrap_or(self.timeout);

        // 2. 只读白名单检查（始终强制执行）
        if let Some(reason) = Self::check_read_only(command) {
            return Err(reason);
        }

        // 3. Shell 探测 + 命令包装
        let shell = self.get_shell();
        let (prog, shell_args) = shell.build_command(command);

        // 4. 构建安全的环境变量
        let env = self.build_env();

        // 5. 通过 ProcessRunner 执行（带超时 + 可选沙箱）
        let runner = ProcessRunner::new(self.work_dir.clone(), timeout)
            .with_env(env);

        let runner = if let Some(spec) = &self.sandbox {
            runner.with_sandbox(spec.clone())
        } else {
            runner
        };

        let output = runner.run(&prog, &shell_args).await;

        // 6. 构建返回结果
        Self::build_tool_result(output)
    }
}


#[async_trait::async_trait]
impl CheckableTool for ReadOnlyBash {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

}

mod tests {
    use super::*;
    use crate::agent::{ActionMode, AgentMode};
    use llm::tool::ToolMeta;

    fn test_sandbox() -> Option<SandboxSpec> {
        None
    }

    fn test_tool() -> ReadOnlyBash {
        ReadOnlyBash::new(
            std::env::current_dir().unwrap(),
            Duration::from_secs(10),
            test_sandbox(),
        )
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        }
    }

    #[tokio::test]
    async fn allowed_ls_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "ls"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(!result.is_err(), "should not be blocked");
        assert!(result.output().len() > 0, "should have output");
    }

    #[tokio::test]
    async fn allowed_cat_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "cat Cargo.toml"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(!result.is_err());
    }

    #[tokio::test]
    async fn allowed_grep_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "grep -r 'fn' src/"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(!result.is_err());
    }

    #[tokio::test]
    async fn blocked_rm_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "rm -rf /tmp"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.is_err(), "should be blocked");
        assert!(result.output().contains("blocked"), "output: {}", result.output());
    }

    #[tokio::test]
    async fn blocked_echo_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "echo 'hello'"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.is_err(), "echo should be blocked");
    }

    #[tokio::test]
    async fn blocked_write_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "touch /tmp/test_file"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.is_err(), "touch should be blocked");
    }

    #[tokio::test]
    async fn blocked_sed_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "sed -i 's/foo/bar/g' file.txt"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.is_err(), "sed should be blocked");
    }

    #[tokio::test]
    async fn allowed_git_log_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "git log --oneline -5"});
        let result = tool.execute(&test_ctx(), &args).await;
        // git log 可能在非 git 目录下失败，但不应被 blocked
        assert!(!result.is_err(), "should not be blocked");
    }

    #[tokio::test]
    async fn missing_command() {
        let tool = test_tool();
        let args = serde_json::json!({});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("missing"));
    }

    #[tokio::test]
    async fn empty_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": ""});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
    }

    #[tokio::test]
    async fn allowed_git_status_command() {
        let tool = test_tool();
        let args = serde_json::json!({"command": "git status --short"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(!result.is_err(), "should not be blocked");
    }

    #[tokio::test]
    async fn allowed_head_tail_wc() {
        let tool = test_tool();
        let ctx = test_ctx();

        let head_result = tool.execute(&ctx, &serde_json::json!({"command": "head -5 Cargo.toml"})).await;
        assert!(!head_result.is_err(), "head should be allowed");

        let tail_result = tool.execute(&ctx, &serde_json::json!({"command": "tail -5 Cargo.toml"})).await;
        assert!(!tail_result.is_err(), "tail should be allowed");

        let wc_result = tool.execute(&ctx, &serde_json::json!({"command": "wc -l Cargo.toml"})).await;
        assert!(!wc_result.is_err(), "wc should be allowed");
    }
}
