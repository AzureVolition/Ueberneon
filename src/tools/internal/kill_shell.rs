// kill_shell.rs —— 终止后台任务。
//
// 通过 JobManager 终止后台任务，先 SIGTERM 后 SIGKILL。

use llm::tool::{Tool, ToolContext, ToolResult};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;
use std::sync::Arc;

use crate::tools::jobs::JobManager;

/// kill_shell — 终止通过 bash(run_in_background=true) 启动的后台任务。
///
/// 发送 SIGTERM 后等待 200ms，然后发送 SIGKILL 确保终止。
#[derive(ToolMetaImpl)]
pub struct KillShell {
    schema: Value,
    read_only: bool,
    job_manager: Arc<JobManager>,
}

impl KillShell {
    pub fn new(job_manager: Arc<JobManager>) -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Background job id to terminate (e.g. 'bg-1')"
                    }
                },
                "required": ["job_id"]
            }),
            read_only: false,
            job_manager,
        }
    }
}

#[async_trait::async_trait]
impl Tool for KillShell {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> ToolResult {
        let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolResult::err("kill_shell: missing required argument 'job_id'"),
        };

        let handle = match self.job_manager.get(job_id) {
            Some(h) => h,
            None => {
                return ToolResult::err(format!(
                    "kill_shell: job '{job_id}' not found"
                ));
            }
        };

        // 收集未读输出（在 kill 前读出）
        let remaining = handle.read_new_output();

        handle.kill().await;

        if remaining.is_empty() {
            ToolResult::ok(format!("job {job_id} terminated"))
        } else {
            ToolResult::ok(format!("job {job_id} terminated\n\nRemaining output:\n{remaining}"))
        }
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
            plan_mode: false,
            progress: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(result.error.is_none(), "error: {:?}", result.error);
        assert!(result.output.contains("terminated"));

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
            plan_mode: false,
            progress: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("not found"));
    }
}
