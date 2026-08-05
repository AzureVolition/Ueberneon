// ── Plan Panel — Floating collapsible task board ──
// Renders from completion_queue, building tree via parent_idx.

use crate::model::{Plan, PlanNode, PlanStatus, StepStatus};
use dioxus::prelude::*;

fn collect_entities(plan: &Plan) -> Vec<crate::model::Entity> {
    if !plan.completion_queue.is_empty() {
        plan.completion_queue
            .iter()
            .flat_map(|qi| qi.batch.iter())
            .cloned()
            .collect()
    } else {
        // 审批阶段：队列为空，从 children 树构建
        PlanNode::to_entities(&plan.children, None)
    }
}

fn roots_from<'a>(entities: &'a [crate::model::Entity]) -> Vec<&'a crate::model::Entity> {
    let mut roots: Vec<&crate::model::Entity> =
        entities.iter().filter(|e| e.parent_idx.is_none()).collect();
    roots.sort_by_key(|e| e.idx);
    roots
}

fn children_of<'a>(entities: &'a [crate::model::Entity], pid: u8) -> Vec<&'a crate::model::Entity> {
    let mut kids: Vec<&crate::model::Entity> = entities
        .iter()
        .filter(|e| e.parent_idx == Some(pid))
        .collect();
    kids.sort_by_key(|e| e.idx);
    kids
}

/// PlanPanel
#[component]
pub fn PlanPanel(
    plan: Option<Plan>,
    on_approve: EventHandler<()>,
    on_reject: EventHandler<()>,
) -> Element {
    let mut is_expanded = use_signal(|| false);

    let plan_data = match plan.as_ref() {
        Some(data) => data,
        None => return rsx! { div { class: "plan-panel-placeholder" } },
    };

    let all_entities = collect_entities(plan_data);
    let roots = roots_from(&all_entities);

    // 有子任务（层级 plan）时统计子任务为步骤；无子任务（扁平 plan，全部为根）时统计全部
    let has_subtasks = all_entities.iter().any(|e| e.parent_idx.is_some());
    let tasks: Vec<&crate::model::Entity> = if has_subtasks {
        all_entities
            .iter()
            .filter(|e| e.parent_idx.is_some())
            .collect()
    } else {
        all_entities.iter().collect()
    };
    let total = tasks.len() as u32;
    let completed = tasks
        .iter()
        .filter(|t| matches!(t.step_status, StepStatus::Completed))
        .count() as u32;
    let progress_pct = if total > 0 {
        (completed as f64 / total as f64 * 100.0) as u32
    } else {
        0
    };

    let mut roots_done: Vec<&crate::model::Entity> = Vec::new();
    let mut roots_doing: Vec<&crate::model::Entity> = Vec::new();
    let mut roots_todo: Vec<&crate::model::Entity> = Vec::new();
    for r in &roots {
        let kids = children_of(&all_entities, r.idx);
        let all_done =
            !kids.is_empty() && kids.iter().all(|k| k.step_status == StepStatus::Completed);
        let any_doing = kids.iter().any(|k| k.step_status == StepStatus::InProgress);
        if all_done {
            roots_done.push(r);
        } else if any_doing {
            roots_doing.push(r);
        } else {
            roots_todo.push(r);
        }
    }

    let doing_count = roots_doing.len();
    let is_need_approval = matches!(plan_data.status, PlanStatus::NeedApproval);

    let approval_summary = if is_need_approval {
        let tops: Vec<&str> = roots.iter().map(|r| r.description.as_str()).collect();
        format!("{} 个阶段待审批 — {}", tops.len(), tops.join(" · "))
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
        let current_task = tasks
            .iter()
            .find(|t| matches!(t.step_status, StepStatus::InProgress));

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
                if mins > 0 {
                    format!("{mins}m {secs}s")
                } else {
                    format!("{secs}s")
                }
            }
            None => "—".to_string(),
        };

        rsx! {
            div { class: "plan-backdrop", onclick: move |_| is_expanded.set(false) }
            div { class: "plan-floating",
                div { class: "plan-floating-header",
                    span { class: "plan-floating-state plan-floating-state--{status_mod}", "plan {status_label}" }
                    span { class: "plan-floating-sep", "·" }
                    span { class: "plan-floating-elapsed", "{elapsed_text}" }
                    div { class: "plan-floating-spacer" }
                    button {
                        class: "plan-floating-close", onclick: move |_| is_expanded.set(false), "✕"
                    }
                }
                p { class: "plan-floating-goal", "{plan_data.goal}" }
                div { class: "plan-floating-progress",
                    div { class: "plan-floating-progress-track",
                        div { class: "plan-floating-progress-fill", style: "width: {progress_pct}%" }
                    }
                    span { class: "plan-floating-progress-label", "{completed}/{total} tasks done" }
                }

                if is_need_approval {
                    div { class: "plan-floating-chips",
                        div { class: "plan-floating-chips-head", "to review ({roots_todo.len()} phases)" }
                        {roots_todo.iter().enumerate().map(|(i, r)| render_chip(r, i, plan_data))}
                    }
                } else {
                    div { class: "plan-floating-board",
                        div { class: "plan-floating-col",
                            div { class: "plan-floating-col-head plan-floating-col-head--done", "done ({roots_done.len()})" }
                            {roots_done.iter().enumerate().map(|(i, r)| render_phase_card(r, i, plan_data))}
                        }
                        div { class: "plan-floating-col",
                            div { class: "plan-floating-col-head plan-floating-col-head--doing", "doing ({roots_doing.len()})" }
                            {roots_doing.iter().enumerate().map(|(i, r)| render_phase_card(r, i, plan_data))}
                        }
                        div { class: "plan-floating-col",
                            div { class: "plan-floating-col-head plan-floating-col-head--todo", "todo ({roots_todo.len()})" }
                            {roots_todo.iter().enumerate().map(|(i, r)| render_phase_card(r, i, plan_data))}
                        }
                    }
                }

                if is_need_approval {
                    div { class: "plan-floating-approval",
                        p { class: "plan-floating-approval-summary", "{approval_summary}" }
                        div { class: "plan-floating-approval-actions",
                            button { class: "plan-approve-btn plan-approve-btn--accept", onclick: move |_| { on_approve.call(()); is_expanded.set(false); }, "✓ 通过审批" }
                            button { class: "plan-approve-btn plan-approve-btn--reject", onclick: move |_| { on_reject.call(()); is_expanded.set(false); }, "✎ 输入修改意见" }
                        }
                    }
                } else {
                    div { class: "plan-floating-status",
                        if let Some(task) = current_task {
                            span { class: "plan-floating-status-arrow", "▸" }
                            span { class: "plan-floating-status-text", "task {task.idx} - {task.description}" }
                            span { class: "plan-floating-status-cursor" }
                        } else if matches!(plan_data.status, PlanStatus::Completed) {
                            span { class: "plan-floating-status-arrow", "✓" }
                            span { class: "plan-floating-status-text", "all tasks completed." }
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

fn render_phase_card(root: &crate::model::Entity, _index: usize, plan: &Plan) -> Element {
    let num = root.idx.to_string();
    let all_entities = collect_entities(plan);
    let kids = children_of(&all_entities, root.idx);
    let all_done = !kids.is_empty() && kids.iter().all(|k| k.step_status == StepStatus::Completed);
    let any_doing = kids.iter().any(|k| k.step_status == StepStatus::InProgress);

    let (status_class, dot_class) = if all_done {
        ("plan-card--done", "plan-card-dot--done")
    } else if any_doing {
        ("plan-card--doing", "plan-card-dot--doing")
    } else {
        ("plan-card--pending", "plan-card-dot--pending")
    };

    rsx! {
        div { class: "plan-card {status_class}",
            div { class: "plan-card-row",
                span { class: "plan-card-dot {dot_class}" }
                span { class: "plan-card-index", "{num}" }
                if all_done {
                    span { class: "plan-card-check", "✓" }
                }
            }
            p { class: "plan-card-desc", "{root.description}" }
            if !kids.is_empty() {
                div { class: "plan-card-subs",
                    {kids.iter().enumerate().map(|(_ci, task)| {
                        let sub_num = format!("{}.{}", num, task.idx);
                        let (dot_class, desc_class) = match task.step_status {
                            StepStatus::Completed => ("plan-sub-dot--done", "plan-sub-desc--done"),
                            StepStatus::InProgress => ("plan-sub-dot--doing", ""),
                            _ => ("plan-sub-dot--pending", ""),
                        };
                        rsx! {
                            div { class: "plan-card-sub",
                                span { class: "plan-sub-dot {dot_class}" }
                                span { class: "plan-sub-index", "{sub_num}" }
                                span { class: "plan-sub-desc {desc_class}", "{task.description}" }
                            }
                        }
                    })}
                }
            }
        }
    }
}

fn render_chip(root: &crate::model::Entity, _index: usize, plan: &Plan) -> Element {
    let num = root.idx.to_string();
    let all_entities = collect_entities(plan);
    let kids = children_of(&all_entities, root.idx);
    rsx! {
        div {
            class: if !kids.is_empty() { "plan-chip plan-chip--has-subs" } else { "plan-chip" },
            div { class: "plan-chip-row",
                span { class: "plan-chip-dot plan-chip-dot--pending" }
                span { class: "plan-chip-index", "{num}" }
                span { class: "plan-chip-desc", "{root.description}" }
            }
            if !kids.is_empty() {
                div { class: "plan-chip-subs",
                    {kids.iter().enumerate().map(|(_ci, task)| {
                        let sub_num = format!("{}.{}", num, task.idx);
                        rsx! {
                            div { class: "plan-chip-sub",
                                span { class: "plan-sub-dot plan-sub-dot--pending" }
                                span { class: "plan-sub-index", "{sub_num}" }
                                span { class: "plan-sub-desc", "{task.description}" }
                            }
                        }
                    })}
                }
            }
        }
    }
}
