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

/// 提交最终计划供用户审批。 
/// 如果步骤过多使用子步骤
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(schema = r##"{
    "type": "object",
    "required": ["plan"],
    "properties": {
        "plan": {
            "type": "object",
            "description": "要创建的计划",
            "required": ["goal", "steps"],
            "additionalProperties": false,
            "properties": {
                "goal": {
                    "type": "string",
                    "description": "计划目标，简明的一句话概括"
                },
                "description": {
                    "type": "string",
                    "description": "计划的详细描述或背景说明"
                },
                "steps": {
                    "type": "array",
                    "description": "执行步骤列表（支持嵌套 children）",
                    "items": {
                        "type": "object",
                        "required": ["index", "description"],
                        "additionalProperties": false,
                        "properties": {
                            "index": {
                                "type": "integer",
                                "minimum": 0,
                                "maximum": 255,
                                "description": "步骤序号，从 0 开始递增"
                            },
                            "description": {
                                "type": "string",
                                "description": "步骤描述：做什么、预期结果"
                            },
                            "children": {
                                "type": "array",
                                "description": "子步骤（递归，与父级结构相同）",
                                "items": { "$ref": "#/properties/plan/properties/steps/items" }
                            }
                        }
                    }
                }
            }
        }
    }
    }"##)]
pub struct CreatePlan;

fn parse_plan(value: &Value) -> Result<Plan, String> {
    // 兼容 LLM 可能将 plan 传为 JSON 字符串而非对象的情况
    let val = match value {
        Value::String(s) => serde_json::from_str(s)
            .map_err(|e| format!("invalid plan (string content): {e}"))?,
        other => other.clone(),
    };
    serde_json::from_value(val).map_err(|e| format!("invalid plan: {e}"))
}

/// 递归重置所有 step 状态为 Pending，并生成临时 id
fn normalize_step(step: &mut ActionStep) {
    step.status = StepStatus::Pending;
    if let Some(children) = &mut step.children {
        for child in children {
            normalize_step(child);
        }
    }
}

/// 将 Plan 和 steps 写入数据库，返回 plan_id
#[allow(dead_code)]
fn persist_plan(
    conn: &rusqlite::Connection,
    project_id: &str,
    conversation_id: &str,
    plan: &Plan,
) -> Result<String, String> {
    use crate::db::metadata::plan::{self as plan_db, PlanStatus as DbPlanStatus};
    

    let db_status: DbPlanStatus = plan.status.clone().into();
    let plan_id = plan_db::create(conn, project_id, conversation_id, &plan.goal, &plan.description, db_status)
        .map_err(|e| format!("db error: {e}"))?;

    // 存储顶级 steps
    for step in &plan.steps {
        persist_step(conn, &plan_id, project_id, None, step)?;
    }

    Ok(plan_id)
}

#[allow(dead_code)]
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
        None,
    )
    .map_err(|e| format!("db error: {e}"))?;

    // 递归存储子 steps
    if let Some(children) = &step.children {
        for child in children {
            persist_step(conn, plan_id, project_id, Some(task_id), child)?;
        }
    }

    Ok(())
}

#[async_trait::async_trait]
impl Tool for CreatePlan {
    async fn execute(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let _project_id = ctx
            .project_id
            .as_deref()
            .ok_or_else(|| "missing project_id in context".to_string())?;
        let _conversation_id = &ctx.main_conversation_id;
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

        // ── 存入运行时 handler（不入库，审批通过后才入库）──
        {
            let mut guard = ctx.handler.current_plan.lock().expect("current_plan lock poisoned");
            *guard = Some(plan.clone());
        }

        Ok(ToolResult::ok("plan created — waiting for approval".to_string()))
    }
}

#[async_trait::async_trait]
impl CheckableTool for CreatePlan {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}
