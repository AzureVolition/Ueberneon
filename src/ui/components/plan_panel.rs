// ── Plan Panel — Kanban board for plan mode task display ──
//
// Renders a 3-column kanban (TODO / DOING / DONE) in a right-side panel.
// Shown when the agent generates a Plan in plan mode.
// Matching the Night Foundry design system (violet + cyan + pink).

use dioxus::prelude::*;

use crate::model::{ActionStep, Difficulty, Plan, PlanStatus, StepStatus};

/// PlanPanel — right-side kanban board for plan mode.
///
/// Shows nothing when `plan` is `None`.
/// When a plan is present, renders header + progress + 3-column board + status bar.
#[component]
pub fn PlanPanel(plan: Option<Plan>) -> Element {
    let plan_data = match plan.as_ref() {
        Some(data) => data,
        None => {
            return rsx! {
                div { class: "plan-panel",
                    div { class: "plan-panel-empty",
                        span { class: "plan-panel-empty-icon", "◉" }
                        p { class: "plan-panel-empty-text",
                            "plan mode active — a task board will appear when the agent builds a plan."
                        }
                    }
                }
            }
        }
    };

    let total = plan_data.steps.len() as u32;
    let completed = plan_data
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Completed))
        .count() as u32;
    let progress_pct = if total > 0 {
        (completed as f64 / total as f64 * 100.0) as u32
    } else {
        0
    };

    let todo_steps: Vec<&ActionStep> = plan_data
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Pending))
        .collect();
    let doing_steps: Vec<&ActionStep> = plan_data
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::InProgress))
        .collect();
    let done_steps: Vec<&ActionStep> = plan_data
        .steps
        .iter()
        .filter(|s| matches!(s.status, StepStatus::Completed | StepStatus::Failed))
        .collect();

    let difficulty_label = match plan_data.difficulty {
        Difficulty::Easy => "easy",
        Difficulty::Medium => "medium",
        Difficulty::Hard => "hard",
    };

    let current_step = doing_steps.first().map(|s| s.description.as_str());

    rsx! {
        div { class: "plan-panel",
            // ── Header ──
            div { class: "plan-header",
                div { class: "plan-header-eyebrow", "plan" }
                h2 { class: "plan-header-goal", "{plan_data.goal}" }
                div { class: "plan-header-meta",
                    span { "difficulty: {difficulty_label}" }
                    span { class: "plan-header-meta-sep", "·" }
                    span { "~{plan_data.estimated_minutes} min" }
                }
            }

            // ── Progress ──
            div { class: "plan-progress",
                div { class: "plan-progress-track",
                    div {
                        class: "plan-progress-fill",
                        style: "width: {progress_pct}%",
                    }
                }
                div { class: "plan-progress-label",
                    "{completed} / {total}"
                }
            }

            // ── Board ──
            div { class: "plan-board",
                // TODO column
                div { class: "plan-column",
                    div { class: "plan-column-header plan-column-header--todo",
                        "todo ({todo_steps.len()})"
                    }
                    {todo_steps.iter().map(|step| render_card(step, plan_data.status.clone()))}
                }
                // DOING column
                div { class: "plan-column",
                    div { class: "plan-column-header plan-column-header--doing",
                        "doing ({doing_steps.len()})"
                    }
                    {doing_steps.iter().map(|step| render_card(step, plan_data.status.clone()))}
                }
                // DONE column
                div { class: "plan-column",
                    div { class: "plan-column-header plan-column-header--done",
                        "done ({done_steps.len()})"
                    }
                    {done_steps.iter().map(|step| render_card(step, plan_data.status.clone()))}
                }
            }

            // ── Status bar ──
            div { class: "plan-status-bar",
                if let Some(desc) = current_step {
                    span { class: "plan-status-bar-arrow", "▸" }
                    span { class: "plan-status-bar-text", "executing step {doing_steps.first().map(|s| s.index).unwrap_or(0)} — {desc}" }
                    span { class: "plan-status-bar-cursor" }
                } else if matches!(plan_data.status, PlanStatus::Completed) {
                    span { class: "plan-status-bar-arrow", "✓" }
                    span { class: "plan-status-bar-text", "all steps completed." }
                } else if matches!(plan_data.status, PlanStatus::Failed) {
                    span { class: "plan-status-bar-arrow", "✗" }
                    span { class: "plan-status-bar-text", "plan execution failed." }
                } else {
                    span { class: "plan-status-bar-arrow", "○" }
                    span { class: "plan-status-bar-text", "waiting to start…" }
                }
            }
        }
    }
}

/// Render a single step card.
fn render_card(step: &ActionStep, _plan_status: PlanStatus) -> Element {
    let (status_class, dot_class) = match step.status {
        StepStatus::Pending => ("plan-card--pending", "plan-card-dot--pending"),
        StepStatus::InProgress => ("plan-card--doing", "plan-card-dot--doing"),
        StepStatus::Completed => ("plan-card--done", "plan-card-dot--done"),
        StepStatus::Failed => ("plan-card--failed", "plan-card-dot--failed"),
    };

    let is_done = matches!(step.status, StepStatus::Completed);

    rsx! {
        div { class: "plan-card {status_class}",
            div { class: "plan-card-row",
                span { class: "plan-card-dot {dot_class}" }
                span { class: "plan-card-index", "{step.index}" }
                if is_done {
                    span { class: "plan-card-check", "✓" }
                }
            }
            p { class: "plan-card-desc", "{step.description}" }
            if let Some(ref hint) = step.tool_hint {
                div { class: "plan-card-hint", "tool: {hint}" }
            }
        }
    }
}
