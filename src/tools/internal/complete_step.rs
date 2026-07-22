// complete_step 工具 —— 标记计划步骤完成。
//
// LLM 在执行模式（有 current_plan）下完成一个步骤后调用，
// 更新 handler.current_plan 中对应 step 状态 + 同步数据库 tasks 表。
// 同时清零 plan.stall_count。

use crate::agent::{ToolContext, Tool, ToolResult};
use crate::model::StepStatus;
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;

use super::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// 标记计划中的一个步骤为已完成。在 Execute Mode 下完成一个步骤后调用，
/// 会自动推进到下一个 Pending 步骤。所有步骤完成后计划标记为 Completed。
#[derive(ToolMetaImpl)]
#[tool(schema = r#"{
    "type": "object",
    "required": ["step_index"],
    "properties": {
        "step_index": {
            "type": "integer",
            "description": "要标记为完成的步骤序号（从 1 开始，对应 Plan 中 step.index）"
        }
    }
}"#)]
pub struct CompleteStep;

/// mark_step_completed 的返回结果
enum MarkResult {
    /// 标记成功
    Completed,
    /// 未找到匹配的 step
    NotFound,
    /// 前面有未完成的步骤，返回它们的 index 列表
    BlockedBy(Vec<u8>),
}

/// 递归在 steps 树中查找并标记 step。
///
/// 标记前检查：同级中所有 index 小于 target_index 的步骤必须已完成，
/// 防止跳过未完成步骤直接完成后面的步骤。
fn mark_step_completed(steps: &mut [crate::model::ActionStep], target_index: u8) -> MarkResult {
    // Phase 1: 不可变查找 — 确认目标存在 + 检查前置步骤
    let mut target_found = false;
    let mut pending: Vec<u8> = Vec::new();
    for step in steps.iter() {
        if step.index == target_index {
            target_found = true;
            pending = steps
                .iter()
                .filter(|s| s.index < target_index && s.status != StepStatus::Completed)
                .map(|s| s.index)
                .collect();
            break;
        }
    }

    if target_found {
        if !pending.is_empty() {
            return MarkResult::BlockedBy(pending);
        }
        // Phase 2: 可变标记
        for step in steps.iter_mut() {
            if step.index == target_index {
                step.status = StepStatus::Completed;
                return MarkResult::Completed;
            }
        }
    }

    // 递归进入子步骤
    for step in steps.iter_mut() {
        if let Some(ref mut children) = step.children {
            match mark_step_completed(children, target_index) {
                MarkResult::NotFound => continue,
                other => return other,
            }
        }
    }

    MarkResult::NotFound
}

/// 更新数据库中对应 task 的状态（通用）
fn update_task_status_in_db_to(
    conn: &rusqlite::Connection,
    plan_id: &str,
    step_index: u8,
    status: &str,
) -> Result<bool, String> {
    let affected = conn
        .execute(
            "UPDATE tasks SET status = ?1 WHERE plan_id = ?2 AND idx = ?3",
            rusqlite::params![status, plan_id, step_index as i32],
        )
        .map_err(|e| format!("db error: {e}"))?;
    Ok(affected > 0)
}

/// 更新数据库中对应 task 的状态为 completed
fn update_task_status_in_db(
    conn: &rusqlite::Connection,
    plan_id: &str,
    step_index: u8,
) -> Result<bool, String> {
    use crate::db::metadata::task::TaskStatus as DbTaskStatus;
    update_task_status_in_db_to(conn, plan_id, step_index, DbTaskStatus::Completed.as_str())
}

/// 在同级 steps 中找第一个 Pending 的步骤设为 InProgress，返回 (index, description)
fn advance_to_next(steps: &mut [crate::model::ActionStep]) -> Option<(u8, String)> {
    for step in steps.iter_mut() {
        if step.status == StepStatus::Pending {
            step.status = StepStatus::InProgress;
            return Some((step.index, step.description.clone()));
        }
    }
    None
}

/// 从 current_plan 获取 plan_id（从 DB 根据 conversation_id 查最新 plan）
fn get_current_plan_id(conn: &rusqlite::Connection, conversation_id: &str) -> Result<Option<String>, String> {
    let mut stmt = conn
        .prepare(
            "SELECT id FROM plans WHERE conversation_id = ?1 ORDER BY created_at DESC LIMIT 1",
        )
        .map_err(|e| format!("db error: {e}"))?;
    let id: Option<String> = stmt
        .query_row(rusqlite::params![conversation_id], |row| row.get(0))
        .ok();
    Ok(id)
}

#[async_trait::async_trait]
impl Tool for CompleteStep {
    async fn execute(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let step_index = args
            .get("step_index")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| "missing 'step_index'".to_string())? as u8;

        // ── 更新内存中的 current_plan ──
        let all_done: bool;
        let next_step: Option<(u8, String)>;
        {
            let mut guard = ctx.handler.current_plan.lock().unwrap();
            let plan = match guard.as_mut() {
                Some(p) => p,
                None => return Err("no active plan to complete step".to_string()),
            };

            match mark_step_completed(&mut plan.steps, step_index) {
                MarkResult::Completed => {}
                MarkResult::NotFound => {
                    return Err(format!("step with index {} not found in current plan", step_index));
                }
                MarkResult::BlockedBy(pending) => {
                    return Err(format!(
                        "cannot complete step {} — prerequisite steps {:?} are still pending. Complete them first.",
                        step_index, pending
                    ));
                }
            }

            // 清零 stall_count
            plan.stall_count = 0;

            // 检查是否所有步骤都完成
            all_done = plan.steps.iter().all(|s| s.status == StepStatus::Completed);
            if all_done {
                plan.status = crate::model::PlanStatus::Completed;
            }

            // ── 自动推进下一步为 InProgress ──
            next_step = advance_to_next(&mut plan.steps);
            if next_step.is_none() {
                plan.status = crate::model::PlanStatus::Completed;
                *guard = None;
            }
        } // guard drop

        // ── 更新数据库 ──
        crate::db::with_db_result(|conn| {
            let pid = get_current_plan_id(conn, &ctx.main_conversation_id)?;
            if let Some(ref pid) = pid {
                update_task_status_in_db(conn, pid, step_index)?;
                // 所有步骤完成，同步更新 plan 状态
                if all_done {
                    crate::db::metadata::plan::mark_completed(conn, pid)
                        .map_err(|e| format!("db error: {e}"))?;
                }
                // 同步新推进的步骤到 DB
                if let Some((next_idx, _)) = next_step {
                    update_task_status_in_db_to(conn, pid, next_idx, "in_progress")?;
                }
            }
            Ok::<_, String>(())
        })
        .map_err(|e| format!("db error: {e}"))?;

        // ── 构造返回消息 ──
        let msg = match next_step {
            Some((idx, ref desc)) => format!("step {} completed. Next: step {} - {}", step_index, idx, desc),
            None => {
                if all_done {
                    
                    format!("step {} completed. All steps done — plan is now Completed.", step_index)
                } else {
                    format!("step {} completed.", step_index)
                }
            }
        };
        Ok(ToolResult::ok(msg))
    }
}

#[async_trait::async_trait]
impl CheckableTool for CompleteStep {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}
