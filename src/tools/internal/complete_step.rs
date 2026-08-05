// complete_step 工具 —— 标记任务完成。
//
// 由 completion_queue 驱动执行，每次取队列头部当前批次完成。

use crate::agent::{ToolContext, ToolResult};
use crate::model::{QueueItemStatus, StepStatus};
use schemars::JsonSchema;
use serde::Deserialize;
use ueberneon_macros::ToolMetaImpl;

use super::common::checkable_tool::CheckableTool;
use crate::db::metadata::task::TaskStatus as DbTaskStatus;
use crate::permission::Decision;

/// 标记计划中的一个任务为已完成。
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(argType = CompleteStepParams)]
pub struct CompleteStep;

/// complete_step 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct CompleteStepParams {
    /// 阶段 idx。
    #[serde(default)]
    #[schemars(
        range(min = 0, max = 255),
        description = "阶段 idx。有阶段分组时传入，纯任务模式不传"
    )]
    parent_idx: Option<u8>,
    /// 任务序号。
    #[schemars(range(min = 1, max = 255), description = "任务序号（同级从 1 开始）")]
    idx: u8,
}

/// 更新 DB 中一批实体的状态为 Completed
fn update_batch_in_db(
    conn: &rusqlite::Connection,
    batch: &[crate::model::Entity],
) -> Result<usize, String> {
    let db_ids: Vec<i64> = batch.iter().filter_map(|e| e.db_id).collect();
    if db_ids.is_empty() {
        return Ok(0);
    }
    let placeholders: Vec<String> = (0..db_ids.len()).map(|i| format!("?{}", i + 2)).collect();
    let sql = format!(
        "UPDATE tasks SET status = ?1 WHERE id IN ({})",
        placeholders.join(",")
    );
    let mut stmt = conn.prepare(&sql).map_err(|e| format!("db error: {e}"))?;
    let mut params: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
    params.push(Box::new(DbTaskStatus::Completed.as_str().to_string()));
    for &id in &db_ids {
        params.push(Box::new(id));
    }
    let refs: Vec<&dyn rusqlite::types::ToSql> = params.iter().map(|p| p.as_ref()).collect();
    stmt.execute(refs.as_slice())
        .map_err(|e| format!("db error: {e}"))
}

/// 更新 DB 中单个实体的状态为 InProgress
fn set_in_progress_in_db(conn: &rusqlite::Connection, db_id: i64) -> Result<usize, String> {
    conn.execute(
        "UPDATE tasks SET status = ?1 WHERE id = ?2",
        rusqlite::params![DbTaskStatus::InProgress.as_str(), db_id],
    )
    .map_err(|e| format!("db error: {e}"))
}

impl CompleteStep {
    /// 工具执行体：参数已由 `GenericsTool` 反序列化为强类型 `CompleteStepParams`。
    async fn do_execute(
        &self,
        ctx: &ToolContext,
        args: &CompleteStepParams,
    ) -> Result<ToolResult, String> {
        let parent_idx = args.parent_idx;
        let idx = args.idx as u8;

        let mut msg_parts: Vec<String> = Vec::new();
        {
            let mut guard = ctx
                .handler
                .current_plan
                .lock()
                .expect("current_plan lock poisoned");
            let plan = match guard.as_mut() {
                Some(p) => p,
                None => return Err("no active plan in progress".to_string()),
            };

            // 找到队列中第一个 Current 的 QueueItem
            let current_pos = plan
                .completion_queue
                .iter()
                .position(|qi| qi.status == QueueItemStatus::Current)
                .ok_or_else(|| "no current task in queue".to_string())?;

            let current_item = &plan.completion_queue[current_pos];
            let head = current_item
                .batch
                .first()
                .ok_or_else(|| "current batch is empty".to_string())?;

            // 检查 (parent_idx, idx) 是否匹配队列头部
            if head.parent_idx != parent_idx || head.idx != idx {
                return Err(format!(
                    "expected task (parent_idx={:?}, idx={}), got (parent_idx={:?}, idx={})",
                    head.parent_idx, head.idx, parent_idx, idx
                ));
            }

            // 完成当前批次：标记所有实体为 Completed
            {
                let item = &mut plan.completion_queue[current_pos];
                for entity in &mut item.batch {
                    entity.step_status = StepStatus::Completed;
                }
                item.status = QueueItemStatus::Completed;
            }

            // 更新 DB
            crate::db::with_db_result(|conn| {
                let completed = &plan.completion_queue[current_pos];
                update_batch_in_db(conn, &completed.batch)?;
                Ok::<_, String>(())
            })
            .map_err(|e| format!("db error: {e}"))?;

            // 记录完成描述
            let descs: Vec<String> = plan.completion_queue[current_pos]
                .batch
                .iter()
                .map(|e| {
                    format!(
                        "{} - {}",
                        if let Some(pid) = e.parent_idx {
                            format!("{}.{}", pid, e.idx)
                        } else {
                            e.idx.to_string()
                        },
                        e.description
                    )
                })
                .collect();
            msg_parts.push(format!("completed: {}", descs.join(", ")));

            // 推进到下一个 Pending 队列项
            let next_pos = plan
                .completion_queue
                .iter()
                .position(|qi| qi.status == QueueItemStatus::Pending);
            match next_pos {
                Some(pos) => {
                    let item = &mut plan.completion_queue[pos];
                    item.status = QueueItemStatus::Current;
                    if let Some(first) = item.batch.first_mut() {
                        first.step_status = StepStatus::InProgress;
                        // 更新 DB
                        if let Some(db_id) = first.db_id {
                            let _ = crate::db::with_db_result(|conn| {
                                set_in_progress_in_db(conn, db_id)
                            });
                        }
                        msg_parts.push(format!("next: task {} - {}", first.idx, first.description));
                    }
                }
                None => {
                    plan.status = crate::model::PlanStatus::Completed;
                    msg_parts.push("All tasks done — plan is now Completed.".to_string());
                    *guard = None;
                }
            }
        }

        // 通知 UI 刷新计划面板
        ctx.handler
            .plan_version
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        Ok(ToolResult::ok(msg_parts.join(" ")))
    }
}

#[async_trait::async_trait]
impl crate::agent::GenericsTool for CompleteStep {
    async fn generics_execute(
        &self,
        ctx: &ToolContext,
        args: &CompleteStepParams,
    ) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for CompleteStep {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}
