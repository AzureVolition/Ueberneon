// ── Plan Panel — Floating collapsible task board ──
//
// Normally hidden. Click the floating pill to expand into a
// floating overlay panel showing the 3-column kanban board.
// Matching the Night Foundry design system (violet + cyan + pink).

use dioxus::prelude::*;

use crate::model::{ActionStep, Plan, PlanStatus, StepStatus};

/// PlanPanel — floating collapsible kanban board for plan mode.
///
/// No plan → renders an invisible placeholder (zero-height).
/// Plan exists + collapsed → floating toggle pill.
/// Plan exists + expanded → floating overlay panel + backdrop.
#[component]
pub fn PlanPanel(plan: Option<Plan>) -> Element {
    let mut is_expanded = use_signal(|| false);

    let plan_data = match plan.as_ref() {
        Some(data) => data,
        None => {
            return rsx! { div { class: "plan-panel-placeholder" } }
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

    let doing_count = doing_steps.len();

    if !*is_expanded.read() {
        // ── Collapsed: floating toggle pill ──
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
        // ── Expanded: floating overlay panel ──
        let current_step = doing_steps.first().map(|s| s.description.as_str());

        let (status_label, status_mod) = match plan_data.status {
            PlanStatus::NeedApproval => ("need approval", "pending"),
            PlanStatus::InProgress => ("in progress", "doing"),
            PlanStatus::Completed => ("completed", "done"),
            PlanStatus::Canceled => ("canceled", "canceled"),
        };

        let elapsed_text = match plan_data.started_at {
            Some(t) => {
                let dur = chrono::Utc::now() - t;
                let mins = dur.num_minutes();
                let secs = dur.num_seconds() % 60;
                if mins > 0 {
                    format!("{mins}m {secs}s")
                } else {
                    format!("{secs}s")
                }
            }
            None => "—".to_string(),
        };

        rsx! {
            // Click-outside backdrop
            div {
                class: "plan-backdrop",
                onclick: move |_| is_expanded.set(false),
            }

            // Floating panel
            div { class: "plan-floating",
                // ── Header row ──
                div { class: "plan-floating-header",
                    // h2 { class: "plan-floating-title", "plan" }
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

                // ── Goal ──
                p { class: "plan-floating-goal", "{plan_data.goal}" }

                // ── Progress bar ──
                div { class: "plan-floating-progress",
                    div { class: "plan-floating-progress-track",
                        div {
                            class: "plan-floating-progress-fill",
                            style: "width: {progress_pct}%",
                        }
                    }
                    span { class: "plan-floating-progress-label",
                        "{completed}/{total} completed"
                    }
                }

                // ── 3-column board ──
                div { class: "plan-floating-board",
                    // DONE column
                    div { class: "plan-floating-col",
                        div { class: "plan-floating-col-head plan-floating-col-head--done",
                            "done ({done_steps.len()})"
                        }
                        {done_steps.iter().map(|step| render_card(step))}
                    }
                    // DOING column
                    div { class: "plan-floating-col",
                        div { class: "plan-floating-col-head plan-floating-col-head--doing",
                            "doing ({doing_steps.len()})"
                        }
                        {doing_steps.iter().map(|step| render_card(step))}
                    }
                    // TODO column
                    div { class: "plan-floating-col",
                        div { class: "plan-floating-col-head plan-floating-col-head--todo",
                            "todo ({todo_steps.len()})"
                        }
                        {todo_steps.iter().map(|step| render_card(step))}
                    }
                }

                // ── Status bar ──
                div { class: "plan-floating-status",
                    if let Some(desc) = current_step {
                        span { class: "plan-floating-status-arrow", "▸" }
                        span { class: "plan-floating-status-text", "{desc}" }
                        span { class: "plan-floating-status-cursor" }
                    } else if matches!(plan_data.status, PlanStatus::Completed) {
                        span { class: "plan-floating-status-arrow", "✓" }
                        span { class: "plan-floating-status-text", "all steps completed." }
                    } else if matches!(plan_data.status, PlanStatus::Canceled) {
                        span { class: "plan-floating-status-arrow", "✗" }
                        span { class: "plan-floating-status-text", "execution failed." }
                    } else {
                        span { class: "plan-floating-status-arrow", "○" }
                        span { class: "plan-floating-status-text", "waiting to start…" }
                    }
                }
            }
        }
    }
}

/// Render a single step card (reused from original).
fn render_card(step: &ActionStep) -> Element {
    let (status_class, dot_class) = match step.status {
        StepStatus::Pending => ("plan-card--pending", "plan-card-dot--pending"),
        StepStatus::InProgress => ("plan-card--doing", "plan-card-dot--doing"),
        StepStatus::Completed => ("plan-card--done", "plan-card-dot--done"),
        StepStatus::Failed => ("plan-card--failed", "plan-card-dot--failed"),
        StepStatus::Bolcked => ("plan-card--blocked", "plan-card-dot--blocked"),
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
