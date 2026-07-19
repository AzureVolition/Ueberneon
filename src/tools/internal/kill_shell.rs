// kill_shell.rs —— 终止后台任务。
//
// 通过 JobManager 终止后台任务，先 SIGTERM 后 SIGKILL。

use crate::agent::{Tool, ToolContext, ToolResult};
#[cfg(test)]
use crate::agent::{AgentMode, ActionMode, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;
use std::sync::{Arc, Mutex};

use crate::tools::jobs::JobManager;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// kill_shell — 终止通过 bash(run_in_background=true) 启动的后台任务。
///
/// 发送 SIGTERM 后等待 200ms，然后发送 SIGKILL 确保终止。
#[derive(ToolMetaImpl)]
#[tool(schema = r#"{"type":"object","properties":{"job_id":{"type":"string","description":"Background job id to terminate (e.g. 'bg-1')"}},"required":["job_id"]}"#)]
pub struct KillShell {
    job_manager: Arc<JobManager>,
}

impl KillShell {
    pub fn new(job_manager: Arc<JobManager>) -> Self {
        Self {
            job_manager,
        }
    }
}

#[async_trait::async_trait]
impl Tool for KillShell {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return Err("kill_shell: missing required argument 'job_id'".into()),
        };

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
            agent_mode: Arc::new(Mutex::new(AgentMode::Ask)),
            progress: None,
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
            agent_mode: Arc::new(Mutex::new(AgentMode::Ask)),
            progress: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("not found"));
    }
}
