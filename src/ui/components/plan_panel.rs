// ── Plan Panel — Floating collapsible task board ──
//
// Normally hidden. Click the floating pill to expand into a
// floating overlay panel showing the 3-column kanban board.
// Matching the Night Foundry design system (violet + cyan + pink).
//
// v4: sub-tasks nested inside parent cards (not separate cards).
//     approval mode shows only top-level todo as horizontal chips.

use dioxus::prelude::*;

use crate::model::{ActionStep, Plan, PlanStatus, StepStatus};

/// PlanPanel — floating collapsible kanban board for plan mode.
#[component]
pub fn PlanPanel(
    plan: Option<Plan>,
    /// 点击"通过审批"时触发
    on_approve: EventHandler<()>,
    /// 点击"输入修改意见"时触发
    on_reject: EventHandler<()>,
) -> Element {
    let mut is_expanded = use_signal(|| false);

    let plan_data = match plan.as_ref() {
        Some(data) => data,
        None => {
            return rsx! { div { class: "plan-panel-placeholder" } }
        }
    };

    // ── 递归收集所有步骤（含子步骤），用于进度统计 ──
    let all_steps = collect_all_steps(&plan_data.steps);

    let total = all_steps.len() as u32;
    let completed = all_steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Completed))
        .count() as u32;
    let progress_pct = if total > 0 {
        (completed as f64 / total as f64 * 100.0) as u32
    } else {
        0
    };

    // 顶层步骤分类
    let top_todo: Vec<&ActionStep> = plan_data
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Pending))
        .collect();
    let top_doing: Vec<&ActionStep> = plan_data
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::InProgress))
        .collect();
    let top_done: Vec<&ActionStep> = plan_data
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Completed | StepStatus::Failed))
        .collect();

    let doing_count = top_doing.len();
    let is_need_approval = matches!(plan_data.status, PlanStatus::NeedApproval);

    // 审批摘要
    let approval_summary = if is_need_approval {
        let tops: Vec<&str> = plan_data
            .steps
            .iter()
            .map(|s| s.description.as_str())
            .collect();
        format!("{} 个步骤待审批 — {}", tops.len(), tops.join(" · "))
    } else {
        String::new()
    };

    if !*is_expanded.read() {
        rsx! {
            button {
                class: "plan-pill",
                onclick: move |_| is_expanded.set(true),
                span { class: "plan-pill-dot" }
                span { class: "plan-pill-text", "plan" }
                span { class: "plan-pill-sep", "·" }
                span { class: "plan-pill-count", "{completed}/{total}" }
                if doing_count > 0 {
                    span { class: "plan-pill-badge", "{doing_count}" }
                }
            }
        }
    } else {
        let current_step = top_doing.first().map(|s| s.description.as_str());

        let (status_label, status_mod) = match plan_data.status {
            PlanStatus::NeedApproval => ("need approval", "pending"),
            PlanStatus::InProgress => ("in progress", "doing"),
            PlanStatus::Completed => ("completed", "done"),
        };

        let elapsed_text = match plan_data.started_at {
            Some(t) => {
                let dur = chrono::Utc::now() - t;
                let mins = dur.num_minutes();
                let secs = dur.num_seconds() % 60;
                if mins > 0 { format!("{mins}m {secs}s") } else { format!("{secs}s") }
            }
            None => "—".to_string(),
        };

        rsx! {
            div { class: "plan-backdrop", onclick: move |_| is_expanded.set(false) }

            div { class: "plan-floating",
                // Header
                div { class: "plan-floating-header",
                    span { class: "plan-floating-state plan-floating-state--{status_mod}", "plan {status_label}" }
                    span { class: "plan-floating-sep", "·" }
                    span { class: "plan-floating-elapsed", "{elapsed_text}" }
                    div { class: "plan-floating-spacer" }
                    button {
                        class: "plan-floating-close",
                        onclick: move |_| is_expanded.set(false),
                        aria_label: "close plan panel",
                        "✕"
                    }
                }

                p { class: "plan-floating-goal", "{plan_data.goal}" }

                // Progress
                div { class: "plan-floating-progress",
                    div { class: "plan-floating-progress-track",
                        div { class: "plan-floating-progress-fill", style: "width: {progress_pct}%" }
                    }
                    span { class: "plan-floating-progress-label", "{completed}/{total} done" }
                }

                // Board
                if is_need_approval {
                    div { class: "plan-floating-chips",
                        div { class: "plan-floating-chips-head", "to review ({top_todo.len()})" }
                        {top_todo.iter().enumerate().map(|(i, s)| render_chip(s, i))}
                    }
                } else {
                    div { class: "plan-floating-board",
                        div { class: "plan-floating-col",
                            div { class: "plan-floating-col-head plan-floating-col-head--done", "done ({top_done.len()})" }
                            {top_done.iter().enumerate().map(|(i, s)| render_card(s, i))}
                        }
                        div { class: "plan-floating-col",
                            div { class: "plan-floating-col-head plan-floating-col-head--doing", "doing ({top_doing.len()})" }
                            {top_doing.iter().enumerate().map(|(i, s)| render_card(s, i))}
                        }
                        div { class: "plan-floating-col",
                            div { class: "plan-floating-col-head plan-floating-col-head--todo", "todo ({top_todo.len()})" }
                            {top_todo.iter().enumerate().map(|(i, s)| render_card(s, i))}
                        }
                    }
                }

                // Approval / Status
                if is_need_approval {
                    div { class: "plan-floating-approval",
                        p { class: "plan-floating-approval-summary", "{approval_summary}" }
                        div { class: "plan-floating-approval-actions",
                            button {
                                class: "plan-approve-btn plan-approve-btn--accept",
                                onclick: move |_| { on_approve.call(()); is_expanded.set(false); },
                                "✓ 通过审批"
                            }
                            button {
                                class: "plan-approve-btn plan-approve-btn--reject",
                                onclick: move |_| { on_reject.call(()); is_expanded.set(false); },
                                "✎ 输入修改意见"
                            }
                        }
                    }
                } else {
                    div { class: "plan-floating-status",
                        if let Some(desc) = current_step {
                            span { class: "plan-floating-status-arrow", "▸" }
                            span { class: "plan-floating-status-text", "{desc}" }
                            span { class: "plan-floating-status-cursor" }
                        } else if matches!(plan_data.status, PlanStatus::Completed) {
                            span { class: "plan-floating-status-arrow", "✓" }
                            span { class: "plan-floating-status-text", "all steps completed." }
                        } else {
                            span { class: "plan-floating-status-arrow", "○" }
                            span { class: "plan-floating-status-text", "waiting to start…" }
                        }
                    }
                }
            }
        }
    }
}

/// 递归收集所有步骤（用于进度统计）。
fn collect_all_steps(steps: &[ActionStep]) -> Vec<&ActionStep> {
    let mut result = Vec::new();
    for step in steps {
        result.push(step);
        if let Some(children) = &step.children {
            result.extend(collect_all_steps(children));
        }
    }
    result
}

/// Kanban 卡片 —— 子任务嵌套在卡片内部。
fn render_card(step: &ActionStep, index: usize) -> Element {
    let num = (index + 1).to_string();

    let (status_class, dot_class) = match step.status {
        StepStatus::Pending => ("plan-card--pending", "plan-card-dot--pending"),
        StepStatus::InProgress => ("plan-card--doing", "plan-card-dot--doing"),
        StepStatus::Completed => ("plan-card--done", "plan-card-dot--done"),
        StepStatus::Failed => ("plan-card--failed", "plan-card-dot--failed"),
        StepStatus::Bolcked => ("plan-card--blocked", "plan-card-dot--blocked"),
    };

    let is_done = matches!(step.status, StepStatus::Completed);
    let has_children = step.children.as_ref().map_or(false, |c| !c.is_empty());

    rsx! {
        div { class: "plan-card {status_class}",
            div { class: "plan-card-row",
                span { class: "plan-card-dot {dot_class}" }
                span { class: "plan-card-index", "{num}" }
                if is_done {
                    span { class: "plan-card-check", "✓" }
                }
            }
            p { class: "plan-card-desc", "{step.description}" }

            if has_children {
                div { class: "plan-card-subs",
                    {
                        let children = step.children.as_ref().unwrap();
                        children.iter().enumerate().map(|(ci, child)| {
                            render_sub_step(child, &num, ci)
                        })
                    }
                }
            }
        }
    }
}

/// 卡片内的子步骤行。
fn render_sub_step(step: &ActionStep, parent_num: &str, child_index: usize) -> Element {
    let sub_num = format!("{}.{}", parent_num, child_index + 1);

    let (dot_class, desc_class) = match step.status {
        StepStatus::Completed => ("plan-sub-dot--done", "plan-sub-desc--done"),
        StepStatus::InProgress => ("plan-sub-dot--doing", ""),
        _ => ("plan-sub-dot--pending", ""),
    };

    rsx! {
        div { class: "plan-card-sub",
            span { class: "plan-sub-dot {dot_class}" }
            span { class: "plan-sub-index", "{sub_num}" }
            span { class: "plan-sub-desc {desc_class}", "{step.description}" }
        }
    }
}

/// 审批模式 chip —— 子任务嵌套在 chip 内部。
fn render_chip(step: &ActionStep, index: usize) -> Element {
    let num = (index + 1).to_string();

    let dot_class = match step.status {
        StepStatus::Pending => "plan-chip-dot--pending",
        _ => "plan-chip-dot--pending",
    };

    let has_children = step.children.as_ref().map_or(false, |c| !c.is_empty());

    rsx! {
        div {
            class: if has_children { "plan-chip plan-chip--has-subs" } else { "plan-chip" },
            // 主行：始终横向
            div { class: "plan-chip-row",
                span { class: "plan-chip-dot {dot_class}" }
                span { class: "plan-chip-index", "{num}" }
                span { class: "plan-chip-desc", "{step.description}" }
            }

            if has_children {
                div { class: "plan-chip-subs",
                    {
                        let children = step.children.as_ref().unwrap();
                        children.iter().enumerate().map(|(ci, child)| {
                            let sub_num = format!("{}.{}", num, ci + 1);
                            rsx! {
                                div { class: "plan-chip-sub",
                                    span { class: "plan-sub-dot plan-sub-dot--pending" }
                                    span { class: "plan-sub-index", "{sub_num}" }
                                    span { class: "plan-sub-desc", "{child.description}" }
                                }
                            }
                        })
                    }
                }
            }
        }
    }
}
