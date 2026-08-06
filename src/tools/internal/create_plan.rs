// create_plan 工具 —— 接收 LLM 生成的计划并持久化。
//
// 使用嵌套 children[] 表示层级。
// 校验规则：同级 idx 从 1 开始连续，最多 2 层。

use crate::agent::{GenericsTool, ToolContext, ToolResult};
use crate::model::{Plan, PlanNode, PlanStatus, StepStatus};
use schemars::JsonSchema;
use serde::Deserialize;
use ueberneon_macros::ToolMetaImpl;

use super::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// 提交最终计划供用户审批。使用嵌套 children[] 表示层级。
///
/// 规则：
/// - 最多 2 层：阶段（根节点）和子任务
/// - 同级节点 idx 从 1 开始连续递增
///
/// 两种模式：
/// - 有阶段分组：children 是阶段列表，每个阶段有 children（子任务列表）
/// - 纯任务模式：children 直接是任务列表（无阶段分组）
///
/// 示例（有阶段）：
/// ```json
/// {"plan": {"goal": "实现用户注册功能",
///   "children": [
///     {"idx": 1, "description": "后端 API 开发", "children": [
///       {"idx": 1, "description": "创建 users 表"},
///       {"idx": 2, "description": "实现 POST /api/register"}
///     ]},
///     {"idx": 2, "description": "前端页面", "children": [
///       {"idx": 1, "description": "注册表单组件"},
///       {"idx": 2, "description": "表单验证逻辑"}
///     ]}
///   ]
/// }}
/// ```
///
/// 示例（纯任务）：
/// ```json
/// {"plan": {"goal": "杂项任务",
///   "children": [
///     {"idx": 1, "description": "任务A"},
///     {"idx": 2, "description": "任务B"},
///     {"idx": 3, "description": "任务C"}
///   ]
/// }}
/// ```
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(argType = CreatePlanParams)]
pub struct CreatePlan;

/// 叶子任务节点（无 children）。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct PlanTaskDef {
    /// 同父下唯一序号（从 1 开始连续）。
    #[schemars(
        range(min = 1, max = 255),
        description = "同父下唯一序号（从 1 开始连续）"
    )]
    idx: u8,
    /// 任务描述。
    #[schemars(description = "任务描述")]
    description: String,
}

/// 计划节点（可递归包含子节点）。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct PlanNodeDef {
    /// 同级唯一序号（从 1 开始连续）。
    #[schemars(
        range(min = 1, max = 255),
        description = "同级唯一序号（从 1 开始连续）"
    )]
    idx: u8,
    /// 阶段或任务的描述。
    #[schemars(description = "阶段或任务的描述")]
    description: String,
    /// 子任务列表。
    #[serde(default)]
    #[schemars(
        description = "子任务列表。有阶段分组时使用，纯任务模式不传。同级 idx 从 1 开始连续"
    )]
    children: Option<Vec<PlanTaskDef>>,
}

/// 计划定义。
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct PlanDef {
    /// 计划目标。
    #[schemars(description = "计划目标，概括本次工作的核心目的")]
    goal: String,
    /// 详细描述（可选）。
    #[serde(default)]
    #[schemars(description = "计划的详细描述（可选）")]
    description: Option<String>,
    /// 顶层节点列表。
    #[schemars(
        description = "顶层节点列表。有阶段分组时为阶段列表，纯任务时为任务列表。同级 idx 从 1 开始连续"
    )]
    children: Vec<PlanNodeDef>,
}

/// create_plan 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct CreatePlanParams {
    /// 要创建的计划。
    #[schemars(description = "要创建的计划，使用嵌套 children[] 表示层级。参考顶部 doc 示例")]
    plan: PlanDef,
}

impl From<PlanTaskDef> for PlanNode {
    fn from(def: PlanTaskDef) -> Self {
        PlanNode {
            idx: def.idx,
            description: def.description,
            children: Vec::new(),
            status: StepStatus::Pending,
        }
    }
}

impl From<PlanNodeDef> for PlanNode {
    fn from(def: PlanNodeDef) -> Self {
        let children = def
            .children
            .unwrap_or_default()
            .into_iter()
            .map(PlanNode::from)
            .collect();
        PlanNode {
            idx: def.idx,
            description: def.description,
            children,
            status: StepStatus::Pending,
        }
    }
}

impl From<PlanDef> for Plan {
    fn from(def: PlanDef) -> Self {
        Plan {
            db_plan_id: None,
            goal: def.goal,
            description: def.description.unwrap_or_default(),
            completion_queue: Vec::new(),
            status: PlanStatus::NeedApproval,
            started_at: None,
            stall_count: 0,
            children: def.children.into_iter().map(PlanNode::from).collect(),
        }
    }
}

/// 递归校验节点列表：同级 idx 从 1 开始连续，最多 2 层
fn validate_nodes(nodes: &[PlanNode], depth: u8) -> Result<(), String> {
    if nodes.is_empty() {
        return Err("children must have at least one item".to_string());
    }

    if depth > 1 {
        return Err("max depth is 2 (phase → task)".to_string());
    }

    // idx 从 1 开始连续
    let mut sorted: Vec<&PlanNode> = nodes.iter().collect();
    sorted.sort_by_key(|n| n.idx);
    if sorted[0].idx != 1 {
        return Err(format!("first idx must be 1, got {}", sorted[0].idx));
    }
    for w in sorted.windows(2) {
        if w[1].idx != w[0].idx + 1 {
            return Err(format!(
                "non-continuous idx: {} and {} under same parent",
                w[0].idx, w[1].idx
            ));
        }
    }

    // 递归校验子节点
    for node in nodes {
        if !node.children.is_empty() {
            validate_nodes(&node.children, depth + 1)?;
        }
    }

    Ok(())
}

/// 递归重置状态为 Pending
fn reset_nodes(nodes: &mut [PlanNode]) {
    for node in nodes.iter_mut() {
        node.status = StepStatus::Pending;
        reset_nodes(&mut node.children);
    }
}

impl CreatePlan {
    /// 工具执行体：参数已由 `GenericsTool` 反序列化为强类型 `CreatePlanParams`。
    async fn do_execute(
        &self,
        ctx: &ToolContext,
        args: &CreatePlanParams,
    ) -> Result<ToolResult, String> {
        // PlanDef → 模型 Plan（From 转换中已初始化状态字段）
        let mut plan: Plan = args.plan.clone().into();

        // 校验
        validate_nodes(&plan.children, 0)?;

        // 重置状态
        plan.status = PlanStatus::NeedApproval;
        plan.stall_count = 0;
        reset_nodes(&mut plan.children);
        plan.started_at = None;

        // ── 存入运行时 handler（不入库，审批通过后才入库）──
        {
            let mut guard = ctx
                .handler
                .current_plan
                .lock()
                .expect("current_plan lock poisoned");
            *guard = Some(plan.clone());
        }

        Ok(ToolResult::ok(
            "plan created — waiting for approval".to_string(),
        ))
    }
}

#[async_trait::async_trait]
impl GenericsTool for CreatePlan {
    async fn generics_execute(
        &self,
        ctx: &ToolContext,
        args: &CreatePlanParams,
    ) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for CreatePlan {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}
