 // ── 主 Agent Plan Mode 前缀注入常量 ──

/// Plan Mode + 无 current_plan：要求制作新计划
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

### Phase 5: Notify User for Approval
Call **CreatePlan** to submit the final plan, then inform the user the plan is ready for review in the UI plan panel.
Do NOT call ExitPlanMode — approval is handled by the user through the UI panel.
Do not execute any actions until the user has approved the plan.
"#;

/// Plan Mode + 有 current_plan：要求修改现有计划
pub const PLAN_MODIFY_PREFIX: &str = r#"[Plan mode — planning workflow]
你当前处于**计划模式**，只能执行只读操作。已有一个现有计划，你的任务是：
1. 审查当前计划并根据用户反馈进行调整
2. 修改完成后调用 **CreatePlan** 工具提交更新后的计划

**不要执行任何写操作或代码修改。**"#;

/// Execute Mode + 有 current_plan：显示步骤进度
pub fn execute_prompt(plan: &crate::model::Plan) -> String {
    let mut steps_display = String::new();
    for step in &plan.steps {
        let icon = match step.status {
            crate::model::StepStatus::Completed => "✅",
            crate::model::StepStatus::InProgress => "🔄",
            crate::model::StepStatus::Pending => "⏳",
            crate::model::StepStatus::Bolcked => "🚫",
            crate::model::StepStatus::Failed => "❌",
        };
        steps_display.push_str(&format!("  {} step {} - {}\n", icon, step.index, step.description));
    }

    let current = plan.steps.iter().find(|s| s.status == crate::model::StepStatus::InProgress);
    let current_line = match current {
        Some(s) => format!("当前步骤: step {} - {}", s.index, s.description),
        None => "当前步骤: 无（所有步骤已完成或尚未开始）".to_string(),
    };

    format!(
        r#"[Execute mode — implementation workflow]
计划目标: {goal}

{current_line}

进度:
{steps}

你处于**执行模式**。请按计划逐步实施：
- 专注于完成当前步骤，完成后调用 **CompleteStep(step_index={current_idx})**
- 所有步骤完成后计划将自动标记为 Completed
- 保持改动聚焦，不要偏离计划"#,
        goal = plan.goal,
        current_line = current_line,
        steps = steps_display,
        current_idx = current.map(|s| s.index).unwrap_or(0),
    )
}

/// stall_count >= 3 时追加的催促提示
pub const STALL_NUDGE_SUFFIX: &str = r#"
⚠️ 你已经连续多轮没有推进计划了。请立即采取行动：
- 如果正在分析，请加速并输出结果
- 如果遇到困难，请说明问题并调整计划
- 尽快完成当前步骤并调用 CompleteStep"#;
