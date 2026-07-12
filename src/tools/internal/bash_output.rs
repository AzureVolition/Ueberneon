// bash_output.rs —— 读取后台任务增量输出。
//
// 通过 JobManager 读取后台任务的 stdout+stderr 增量。
// 每次调用返回自上次读取以来的新内容。

use llm::tool::{Tool, ToolContext, ToolResult};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;
use std::sync::Arc;

use crate::tools::jobs::JobManager;

/// bash_output — 读取通过 bash(run_in_background=true) 启动的后台任务输出。
///
/// 返回自上次调用以来产生的新文本。当任务已结束时标记 finished。
#[derive(ToolMetaImpl)]
pub struct BashOutput {
    schema: Value,
    read_only: bool,
    job_manager: Arc<JobManager>,
}

impl BashOutput {
    pub fn new(job_manager: Arc<JobManager>) -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Background job id returned by bash (e.g. 'bg-1')"
                    }
                },
                "required": ["job_id"]
            }),
            read_only: true,
            job_manager,
        }
    }
}

#[async_trait::async_trait]
impl Tool for BashOutput {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> ToolResult {
        let job_id = match args.get("job_id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => return ToolResult::err("bash_output: missing required argument 'job_id'"),
        };

        let handle = match self.job_manager.get(job_id) {
            Some(h) => h,
            None => {
                return ToolResult::err(format!(
                    "bash_output: job '{job_id}' not found (it may have been reaped or never existed)"
                ));
            }
        };

        let output = handle.read_new_output();
        let finished = handle.finished.load(std::sync::atomic::Ordering::SeqCst);

        if output.is_empty() && finished {
            let exit_code = handle.exit_code.load(std::sync::atomic::Ordering::SeqCst);
            if exit_code == 0 {
                ToolResult::ok(format!("job {job_id} finished successfully (exit 0)"))
            } else {
                ToolResult::ok(format!(
                    "job {job_id} finished with exit code {exit_code}"
                ))
            }
        } else if output.is_empty() {
            ToolResult::ok(format!("job {job_id} is still running (no new output)"))
        } else if finished {
            ToolResult::ok(format!(
                "{output}",
            ))
        } else {
            ToolResult::ok(format!(
                "{output}\n[job {job_id} still running]"
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn read_output_from_background_job() {
        let mgr = Arc::new(JobManager::new());
        let job_id = mgr
            .spawn("echo", &["bg_test".into()], &std::env::current_dir().unwrap())
            .await;

        tokio::time::sleep(Duration::from_millis(300)).await;

        let tool = BashOutput::new(mgr.clone());
        let args = serde_json::json!({"job_id": job_id});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            progress: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(
            result.output.contains("bg_test"),
            "output: {}",
            result.output
        );
    }

    #[tokio::test]
    async fn unknown_job_id() {
        let mgr = Arc::new(JobManager::new());
        let tool = BashOutput::new(mgr);
        let args = serde_json::json!({"job_id": "bg-99999"});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            progress: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(result.error.is_some());
        assert!(result.error.unwrap().contains("not found"));
    }

    #[tokio::test]
    async fn missing_job_id() {
        let mgr = Arc::new(JobManager::new());
        let tool = BashOutput::new(mgr);
        let args = serde_json::json!({});
        let ctx = ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            progress: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(result.error.is_some());
    }
}
