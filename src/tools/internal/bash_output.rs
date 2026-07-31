// bash_output.rs —— 读取后台任务增量输出。
//
// 通过 JobManager 读取后台任务的 stdout+stderr 增量。
// 每次调用返回自上次读取以来的新内容。

use crate::agent::{ToolContext, ToolResult, GenericsTool};
#[cfg(test)]
use crate::agent::{AgentHandler, ActionMode, ToolResultExt};
use ueberneon_macros::ToolMetaImpl;
use serde::Deserialize;
use schemars::JsonSchema;
use std::sync::Arc;

use crate::tools::jobs::JobManager;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// bash_output — 读取通过 bash(run_in_background=true) 启动的后台任务输出。
///
/// 返回自上次调用以来产生的新文本。当任务已结束时标记 finished。
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(argType = BashOutputParams)]
pub struct BashOutput {
    job_manager: Arc<JobManager>,
}

/// bash_output 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct BashOutputParams {
    /// 后台任务 ID。
    #[schemars(description = "Background job id returned by bash (e.g. 'bg-1')")]
    job_id: String,
}

impl BashOutput {
    pub fn new(job_manager: Arc<JobManager>) -> Self {
        Self {
            job_manager,
        }
    }

    async fn do_execute(&self, _ctx: &ToolContext, args: &BashOutputParams) -> Result<ToolResult, String> {
        let handle = match self.job_manager.get(&args.job_id) {
            Some(h) => h,
            None => {
                return Err(format!(
                    "bash_output: job '{}' not found (it may have been reaped or never existed)",
                    args.job_id
                ));
            }
        };

        let output = handle.read_new_output();
        let finished = handle.finished.load(std::sync::atomic::Ordering::SeqCst);

        if output.is_empty() && finished {
            let exit_code = handle.exit_code.load(std::sync::atomic::Ordering::SeqCst);
            if exit_code == 0 {
                Ok(ToolResult::ok(format!("job {} finished successfully (exit 0)", args.job_id)))
            } else {
                Ok(ToolResult::ok(format!(
                    "job {} finished with exit code {exit_code}", args.job_id
                )))
            }
        } else if output.is_empty() {
            Ok(ToolResult::ok(format!("job {} is still running (no new output)", args.job_id)))
        } else if finished {
            Ok(ToolResult::ok(format!("{output}")))
        } else {
            Ok(ToolResult::ok(format!("{output}\n[job {} still running]", args.job_id)))
        }
    }
}

#[async_trait::async_trait]
impl GenericsTool for BashOutput {
    async fn generics_execute(&self, ctx: &ToolContext, args: &BashOutputParams) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for BashOutput {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Tool;
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
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        cancel_token: None,
        };

        let result = tool.execute(&ctx, &args).await;
        assert!(
            result.output().contains("bg_test"),
            "output: {}",
            result.output()
        );
    }

    #[tokio::test]
    async fn unknown_job_id() {
        let mgr = Arc::new(JobManager::new());
        let tool = BashOutput::new(mgr);
        let args = serde_json::json!({"job_id": "bg-99999"});
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

    #[tokio::test]
    async fn missing_job_id() {
        let mgr = Arc::new(JobManager::new());
        let tool = BashOutput::new(mgr);
        let args = serde_json::json!({});
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
    }
}
