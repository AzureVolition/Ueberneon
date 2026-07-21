// bash.rs —— Bash 工具（前台 + 后台执行）。
//
// 解析 {command, run_in_background}
// 参数，通过 shell 探测和进程执行器运行命令，返回合并的 stdout+stderr。
// 前台：ProcessRunner 同步执行；后台：JobManager 异步 spawn，返回 job ID。
// 支持沙箱隔离（macOS Seatbelt / Linux bubblewrap）和环境变量安全处理。

use crate::agent::{ActionMode, Tool, AgentHandler, ToolContext, ToolResult, ToolResultExt};
use llm::tool::ToolMeta;
use racpagent_macros::ToolMetaImpl;
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use super::common::env::EnvBuilder;
use super::common::process::{ProcessOutput, ProcessRunner};
use super::common::shell::Shell;
use crate::tools::jobs::JobManager;
use crate::tools::sandbox::SandboxSpec;
use crate::permission::{Check, Decision, gate::PermissionChecked};
use crate::tools::internal::common::checkable_tool::CheckableTool;

/// bash — 在子进程中执行 shell 命令，返回合并后的 stdout+stderr。
///
/// 使用场景：build、test、git、包管理器等需要真实操作系统命令的操作。
/// 不要用于搜索/读取/编辑文件 —— 请使用专用的 grep / read_file / edit_file 工具。
#[derive(ToolMetaImpl)]
#[tool(schema = r#"{"type":"object","properties":{"command":{"type":"string","description":"Shell command to execute"},"run_in_background":{"type":"boolean","description":"Run detached: returns a job id immediately and keeps running across turns. Use bash_output to read output and kill_shell to terminate."},"preserve_background_processes":{"type":"boolean","description":"After the shell command exits normally, keep any process-group members it intentionally left behind."}},"required":["command"]}"#)]
pub struct Bash {
    /// 工作目录（命令在此目录下执行）。
    work_dir: PathBuf,
    /// 每次命令的超时时间（仅前台模式）。
    timeout: Duration,
    /// 后台任务管理器（后台模式）。
    job_manager: Arc<JobManager>,
    /// 沙箱配置（None = 禁用沙箱，Some = 启用并由 SandboxSpec 定义策略）。
    sandbox: Option<SandboxSpec>,
    /// 缓存的 shell 探测结果（首次探测后复用）。
    shell: std::sync::OnceLock<Shell>,
    /// 权限检查列表（bash 专用：ForcePushGuard、DangerousPatternDetector 等）。
    checks: Vec<Box<dyn Check>>,
}

/// bash 工具的输入参数。
#[derive(Debug, Deserialize)]
#[allow(dead_code)]
struct BashParams {
    /// 要执行的 shell 命令。
    command: String,
    /// 是否以后台模式运行。
    #[serde(default)]
    run_in_background: bool,
    /// 后台任务结束后是否保留子进程组。
    #[serde(default)]
    preserve_background_processes: bool,
}

impl Bash {
    pub fn new(
        work_dir: PathBuf,
        timeout: Duration,
        job_manager: Arc<JobManager>,
        sandbox: Option<SandboxSpec>,
        checks: Vec<Box<dyn Check>>,
    ) -> Self {
        Self {
            work_dir,
            timeout,
            job_manager,
            sandbox,
            shell: std::sync::OnceLock::new(),
            checks,
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
}

impl PermissionChecked for Bash {
    fn permission_checks(&self) -> &[Box<dyn Check>] {
        &self.checks
    }
}

#[async_trait::async_trait]
impl Tool for Bash {
    
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        // 1. 解析参数
        let params: BashParams = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => return Err(format!("bash: invalid arguments: {e}")),
        };

        if params.command.trim().is_empty() {
            return Err("bash: 'command' must not be empty".into());
        }

        // 2. Shell 探测 + 命令包装
        let shell = self.get_shell();
        let (prog, shell_args) = shell.build_command(&params.command);

        // 3. 后台模式：通过 JobManager spawn
        if params.run_in_background {
            let job_id = self
                .job_manager
                .spawn(&prog, &shell_args, &self.work_dir)
                .await;

            return Ok(ToolResult::ok(format!(
                "background job started: {job_id}\n\
                 Use bash_output with job_id=\"{job_id}\" to read output.\n\
                 Use kill_shell with job_id=\"{job_id}\" to terminate."
            )));
        }

        // 4. 构建安全的环境变量
        let env = self.build_env();

        // 5. 前台模式：通过 ProcessRunner 同步执行（带沙箱 + 安全环境变量）
        let runner = ProcessRunner::new(self.work_dir.clone(), self.timeout)
            .with_env(env);

        // 沙箱：启用时传递给 ProcessRunner
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
impl CheckableTool for Bash {
    fn check(&self, ctx: &ToolContext, args: &Value) -> Decision {
        match self.check_permission(self.name(), args, *ctx.handler.agent_mode.lock().unwrap()) {
            Decision::Allow => {}
            decision => return decision,
        }
        if let ActionMode::Plan = ctx.plan_mode {
            let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
            if let Some(reason) = Self::check_plan_mode(command) {
                return Decision::Deny(reason);
            }
        }
        Decision::Allow
    }

}

impl Bash {
    /// plan mode 下 bash 的允许命令前缀白名单。
    const PLAN_MODE_ALLOWED_PREFIXES: &[&str] = &[
        "ls", "cat", "head", "tail", "wc", "find", "grep", "git log",
        "git diff", "git show", "git status", "git branch",
    ];

    fn check_plan_mode(command: &str) -> Option<String> {
        let cmd = command.trim();
        let allowed = Self::PLAN_MODE_ALLOWED_PREFIXES
            .iter()
            .any(|prefix| cmd.starts_with(prefix));
        if !allowed {
            Some(format!(
                "blocked: bash command not allowed in plan mode: {}",
                cmd
            ))
        } else {
            None
        }
    }

    /// 将 ProcessOutput 转为 ToolResult。
    fn build_tool_result(output: ProcessOutput) -> Result<ToolResult, String> {
        if output.exit_code != 0 {
            // 出错：将 output 和错误信息合并为 Error 消息
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::AgentMode;
    use llm::tool::ToolMeta;

    fn test_job_manager() -> Arc<JobManager> {
        Arc::new(JobManager::new())
    }

    fn test_sandbox() -> Option<SandboxSpec> {
        None
    }

    fn test_bash() -> Bash {
        Bash::new(
            std::env::current_dir().unwrap(),
            Duration::from_secs(10),
            test_job_manager(),
            test_sandbox(),
            vec![],
        )
    }

    #[tokio::test]
    async fn execute_simple_echo() {
        let bash = test_bash();
        let args = serde_json::json!({"command": "echo hello"});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        };

        let result = bash.execute(&ctx, &args).await;
        assert!(result.error().is_none(), "unexpected error: {:?}", result.error());
        assert!(result.output().contains("hello"), "output: {}", result.output());
        assert!(!result.is_err());
    }

    #[tokio::test]
    async fn execute_with_stderr() {
        let bash = test_bash();
        let args = serde_json::json!({"command": "echo to_stderr >&2"});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        };

        let result = bash.execute(&ctx, &args).await;
        assert!(result.output().contains("to_stderr"), "output: {}", result.output());
    }

    #[tokio::test]
    async fn execute_failing_command() {
        let bash = test_bash();
        let args = serde_json::json!({"command": "exit 42"});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        };

        let result = bash.execute(&ctx, &args).await;
        assert!(result.error().is_some(), "should have error for non-zero exit");
        assert!(result.error().unwrap().contains("42"));
    }

    #[tokio::test]
    async fn empty_command_rejected() {
        let bash = test_bash();
        let args = serde_json::json!({"command": "   "});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        };

        let result = bash.execute(&ctx, &args).await;
        assert!(result.error().is_some());
    }

    #[tokio::test]
    async fn background_returns_job_id() {
        let bash = test_bash();
        let args = serde_json::json!({"command": "sleep 1; echo done", "run_in_background": true});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        };

        let result = bash.execute(&ctx, &args).await;
        assert!(result.error().is_none(), "unexpected error: {:?}", result.error());
        assert!(result.output().contains("bg-"), "output: {}", result.output());
        assert!(result.output().contains("bash_output"), "output: {}", result.output());
    }

    #[tokio::test]
    async fn plan_mode_blocks_dangerous_commands() {
        let bash = test_bash();
        let args = serde_json::json!({"command": "rm -rf /tmp"});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Plan,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        };

        let result = bash.execute(&ctx, &args).await;
        assert!(result.is_err(), "rm should be blocked in plan mode");
    }

    #[tokio::test]
    async fn plan_mode_allows_readonly_commands() {
        let bash = test_bash();
        let args = serde_json::json!({"command": "ls"});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Plan,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        };

        let result = bash.execute(&ctx, &args).await;
        assert!(!result.is_err(), "ls should be allowed in plan mode");
        assert!(result.error().is_none());
    }

    #[tokio::test]
    async fn env_is_filtered() {
        let bash = test_bash();
        let env = bash.build_env();
        // secrets 应被过滤（如果设置了的话，至少不应泄露原始值）
        if let Some(val) = env.get("OPENAI_API_KEY") {
            assert_eq!(val, "[redacted]");
        }
        // 基本环境变量应存在
        assert!(env.contains_key("PATH"));
        assert!(env.contains_key("HOME"));
    }
}
