pub const PLAN_CREATE_PREFIX: &str = r#"

---
Plan mode is active. The user indicated that they do not want you to execute yet -- you MUST NOT make any edits, run any non-readonly tools (including changing configs or making commits), or otherwise make any changes to the system. This supersedes any other instructions you have received.

You should build your plan incrementally by writing to or editing a plan file. NOTE that this is the only file you are allowed to edit - other than this you are only allowed to take READ-ONLY actions.

---

## 5-Phase Plan Workflow

### Phase 1: Initial Understanding
Goal: Gain a comprehensive understanding of the user's request by reading through code and asking them questions.
1. Focus on understanding the user's request and the code associated with their request.
2. Launch Explore agents with Task tool to efficiently explore the codebase.
3. After exploring the code, ask clarifying questions to resolve ambiguities.

### Phase 2: Design
Goal: Design an implementation approach based on the user's intent and your exploration results.
- Consider alternatives and validate your understanding.
- Produce a detailed implementation plan with file paths and code traces.

### Phase 3: Review
Goal: Review the plan and ensure alignment with the user's intentions.
1. Read critical files to deepen understanding.
2. Ensure plans align with the user's original request.

### Phase 4: Final Plan
Goal: Write the final plan.
- Include only the recommended approach, not all alternatives.
- Keep it concise but detailed enough to execute.
- Include paths of critical files to be modified.
- Include a verification section describing how to test changes end-to-end.
- When calling **CreatePlan**, pass a `plan` object with:
  - `goal`: 计划目标
  - `description`: 详细描述（可选）
  - `children`: 嵌套数组。有阶段分组时 children 是阶段列表，每个阶段有 children（子任务）；纯任务模式 children 直接是任务列表
  - 同级 idx 从 1 开始连续
  - 最多 2 层

### Phase 5: Notify User for Approval
Call **CreatePlan** to submit the final plan, then inform the user the plan is ready for review in the UI plan panel.
Do NOT call ExitPlanMode — approval is handled by the user through the UI panel.
Do not execute any actions until the user has approved the plan.
"#;

pub const PLAN_MODIFY_PREFIX: &str = r#"[Plan mode — planning workflow]
你当前处于**计划模式**，只能进行只读探索和计划修改。

已有一个现有计划等待用户通过 UI 面板审批。在你的视角中：
- 计划的状态仍然是 **等待审批**（NeedApproval）
- **不得自行开始执行计划中的任何任务**
- **不得调用 Bash、ReadFile 等执行工具**——你不是在执行，而是在规划
- 你的唯一任务是：审查计划、按用户反馈修改计划、调用 **CreatePlan** 提交更新

**用户通过 UI 点击"通过审批"之前，禁止执行任何非只读工具。**"#;

/// Execute Mode 显示 — 从 completion_queue 重建树并渲染
pub fn execute_prompt(plan: &crate::model::Plan) -> String {
    use crate::model::{QueueItemStatus, StepStatus};

    // 从队列收集所有实体
    let all_entities: Vec<&crate::model::Entity> = plan
        .completion_queue
        .iter()
        .flat_map(|qi| qi.batch.iter())
        .collect();

    // 按 parent_idx 分组建树
    let roots: Vec<&&crate::model::Entity> = all_entities
        .iter()
        .filter(|e| e.parent_idx.is_none())
        .collect();
    let children_of = |pid: u8| -> Vec<&&crate::model::Entity> {
        let mut kids: Vec<&&crate::model::Entity> = all_entities
            .iter()
            .filter(|e| e.parent_idx == Some(pid))
            .collect();
        kids.sort_by_key(|e| e.idx);
        kids
    };

    let mut display = String::new();

    // 当前 Current 的 QueueItem
    let current_entity = plan
        .completion_queue
        .iter()
        .find(|qi| qi.status == QueueItemStatus::Current)
        .and_then(|qi| qi.batch.first());

    let current_line = match current_entity {
        Some(e) => format!("当前任务: task {} - {}", e.idx, e.description),
        None => "当前任务: 无（所有任务已完成或尚未开始）".to_string(),
    };

    let next_pending = plan
        .completion_queue
        .iter()
        .find(|qi| qi.status == QueueItemStatus::Pending)
        .and_then(|qi| qi.batch.first());
    let current_idx = current_entity
        .map(|e| e.idx)
        .or_else(|| next_pending.map(|e| e.idx))
        .unwrap_or(0);

    // 显示树
    for root in &roots {
        let kids = children_of(root.idx);
        let all_done = kids.iter().all(|k| k.step_status == StepStatus::Completed);
        let root_icon = if all_done && !kids.is_empty() {
            "✅"
        } else {
            "📋"
        };
        display.push_str(&format!(
            "{} {} - {}\n",
            root_icon, root.idx, root.description
        ));

        for child in &kids {
            let icon = match child.step_status {
                StepStatus::Completed => "  ✅",
                StepStatus::InProgress => "  🔄",
                StepStatus::Pending => "  ⏳",
                StepStatus::Bolcked => "  🚫",
                StepStatus::Failed => "  ❌",
            };
            display.push_str(&format!(
                "{} task {} - {}\n",
                icon, child.idx, child.description
            ));
        }
    }

    // 如果 roots 为空（纯任务模式），直接显示所有实体
    if roots.is_empty() {
        let mut sorted: Vec<&&crate::model::Entity> = all_entities.iter().collect();
        sorted.sort_by_key(|e| e.idx);
        for entity in &sorted {
            let icon = match entity.step_status {
                StepStatus::Completed => "✅",
                StepStatus::InProgress => "🔄",
                StepStatus::Pending => "⏳",
                StepStatus::Bolcked => "🚫",
                StepStatus::Failed => "❌",
            };
            display.push_str(&format!(
                "{} task {} - {}\n",
                icon, entity.idx, entity.description
            ));
        }
    }

    let help_line = if let Some(e) = current_entity {
        let parent_hint = e
            .parent_idx
            .map(|p| format!(", parent_idx={}", p))
            .unwrap_or_default();
        format!(
            "- 专注于完成当前任务 task {}（{}），完成后调用 **CompleteStep(idx={}{})**",
            e.idx, e.description, e.idx, parent_hint
        )
    } else {
        format!("- 完成后调用 **CompleteStep(idx={})**", current_idx)
    };

    format!(
        r#"[Execute mode — implementation workflow]
计划目标: {goal}

{current_line}

进度:
{display}
你处于**执行模式**。请按计划逐步实施：
{help_line}
- 所有任务完成后计划将自动标记为 Completed
- 保持改动聚焦，不要偏离计划"#,
        goal = plan.goal,
        current_line = current_line,
        display = display,
        help_line = help_line,
    )
}

pub const STALL_NUDGE_SUFFIX: &str = r#"
⚠️ 你已经连续多轮没有推进计划了。请立即采取行动：
- 如果正在分析，请加速并输出结果
- 如果遇到困难，请说明问题并调整计划
- 尽快完成当前任务并调用 CompleteStep"#;
