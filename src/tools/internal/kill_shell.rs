// kill_shell.rs —— 终止后台任务。
//
// 通过 JobManager 终止后台任务，先 SIGTERM 后 SIGKILL。

use crate::agent::{GenericsTool, ToolContext, ToolResult};
#[cfg(test)]
use crate::agent::{ActionMode, AgentHandler, Tool, ToolResultExt};
use ueberneon_macros::ToolMetaImpl;
use serde::Deserialize;
use schemars::JsonSchema;
use std::sync::Arc;

use crate::tools::jobs::JobManager;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// kill_shell — 终止通过 bash(run_in_background=true) 启动的后台任务。
///
/// 发送 SIGTERM 后等待 200ms，然后发送 SIGKILL 确保终止。
#[derive(ToolMetaImpl)]
#[tool(argType = KillShellParams)]
pub struct KillShell {
    job_manager: Arc<JobManager>,
}

/// kill_shell 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct KillShellParams {
    /// 后台任务 ID。
    #[schemars(description = "Background job id to terminate (e.g. 'bg-1')")]
    job_id: String,
}

impl KillShell {
    pub fn new(job_manager: Arc<JobManager>) -> Self {
        Self {
            job_manager,
        }
    }

    async fn do_execute(&self, _ctx: &ToolContext, args: &KillShellParams) -> Result<ToolResult, String> {
        let job_id = &args.job_id;

        let handle = match self.job_manager.get(job_id) {
            Some(h) => h,
            None => {
                return Err(format!(
                    "kill_shell: job '{job_id}' not found"
                ));
            }
        };

        // 收集未读输出（在 kill 前读出）
        let remaining = handle.read_new_output();

        handle.kill().await;

        if remaining.is_empty() {
            Ok(ToolResult::ok(format!("job {job_id} terminated")))
        } else {
            Ok(ToolResult::ok(format!("job {job_id} terminated\n\nRemaining output:\n{remaining}")))
        }
    }
}

#[async_trait::async_trait]
impl GenericsTool for KillShell {
    async fn generics_execute(&self, ctx: &ToolContext, args: &KillShellParams) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for KillShell {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn kill_running_job() {
        let mgr = Arc::new(JobManager::new());
        let job_id = mgr
            .spawn("sleep", &["10".into()], &std::env::current_dir().unwrap())
            .await;

        let tool = KillShell::new(mgr.clone());
        let args = serde_json::json!({"job_id": job_id});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        cancel_token: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("terminated"));

        // 验证 job 已结束
        let handle = mgr.get(&job_id).unwrap();
        assert!(handle.finished.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn kill_nonexistent_job() {
        let mgr = Arc::new(JobManager::new());
        let tool = KillShell::new(mgr);
        let args = serde_json::json!({"job_id": "bg-404"});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        cancel_token: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("not found"));
    }
}
