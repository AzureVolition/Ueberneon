// create_plan 工具 —— 接收 LLM 生成的计划并持久化。
//
// 将 Plan（含多级 step）存入运行时 handler.current_plan 和数据库。
// 所有 step 的状态会被重置为 pending，plan 状态为 need_approval。

use crate::agent::{ToolContext, Tool, ToolResult};
use crate::model::{ActionStep, Plan, PlanStatus, StepStatus};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;

use super::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

#[derive(ToolMetaImpl)]
#[tool(schema = r#"{
    "type": "object",
    "required": ["project_id", "conversation_id", "plan"],
    "properties": {
        "project_id": { "type": "string", "description": "所属项目 ID" },
        "conversation_id": { "type": "string", "description": "所属对话 ID" },
        "plan": {
            "type": "object",
            "description": "Plan 结构体（goal + steps，steps 可含 children）"
        }
    }
}"#)]
pub struct CreatePlan;

fn parse_plan(value: &Value) -> Result<Plan, String> {
    serde_json::from_value(value.clone()).map_err(|e| format!("invalid plan: {e}"))
}

/// 递归重置所有 step 状态为 Pending，并生成临时 id
fn normalize_step(step: &mut ActionStep) {
    step.status = StepStatus::Pending;
    for child in &mut step.children {
        normalize_step(child);
    }
}

/// 将 Plan 和 steps 写入数据库，返回 plan_id
fn persist_plan(
    conn: &rusqlite::Connection,
    project_id: &str,
    conversation_id: &str,
    plan: &Plan,
) -> Result<String, String> {
    use crate::db::metadata::plan::{self as plan_db, PlanStatus as DbPlanStatus};
    use crate::db::metadata::task::{self as task_db, TaskStatus as DbTaskStatus};

    let db_status: DbPlanStatus = plan.status.clone().into();
    let plan_id = plan_db::create(conn, project_id, conversation_id, &plan.goal, "", db_status)
        .map_err(|e| format!("db error: {e}"))?;

    // 存储顶级 steps
    for step in &plan.steps {
        persist_step(conn, &plan_id, project_id, None, step)?;
    }

    Ok(plan_id)
}

fn persist_step(
    conn: &rusqlite::Connection,
    plan_id: &str,
    project_id: &str,
    parent_id: Option<i64>,
    step: &ActionStep,
) -> Result<(), String> {
    use crate::db::metadata::task::{self as task_db, TaskStatus as DbTaskStatus};

    let db_status: DbTaskStatus = step.status.clone().into();
    let task_id = task_db::create(
        conn,
        plan_id,
        project_id,
        parent_id,
        step.index as i32,
        &step.description,
        db_status,
        step.tool_hint.as_deref(),
    )
    .map_err(|e| format!("db error: {e}"))?;

    // 递归存储子 steps
    for child in &step.children {
        persist_step(conn, plan_id, project_id, Some(task_id), child)?;
    }

    Ok(())
}

#[async_trait::async_trait]
impl Tool for CreatePlan {
    async fn execute(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let project_id = args
            .get("project_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'project_id'".to_string())?;
        let conversation_id = args
            .get("conversation_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'conversation_id'".to_string())?;
        let plan_val = args
            .get("plan")
            .ok_or_else(|| "missing 'plan'".to_string())?;

        let mut plan = parse_plan(plan_val)?;

        // 重置状态
        plan.status = PlanStatus::NeedApproval;
        plan.stall_count = 0;
        for step in &mut plan.steps {
            normalize_step(step);
        }
        plan.started_at = None;

        // ── 存入运行时 handler ──
        {
            let mut guard = ctx.handler.current_plan.lock().unwrap();
            *guard = Some(plan.clone());
        }

        // ── 存入数据库 ──
        crate::db::with_db_result(|conn| {
            persist_plan(conn, project_id, conversation_id, &plan)
        })
        .map_err(|e| format!("db error: {e}"))?;

        Ok(ToolResult::ok("plan created and saved".to_string()))
    }
}

#[async_trait::async_trait]
impl CheckableTool for CreatePlan {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}
