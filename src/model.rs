// ── 核心数据模型 ──
//
// 独立于 UI 层，供 agent、store、ui 等模块共享。
// 从 src/ui/state.rs 迁移而来。

use chrono::{DateTime, Local, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};

/// 消息角色
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub enum Role {
    User,
    Assistant,
    System,
}

/// 工具调用状态
#[derive(Clone, PartialEq, Debug, Serialize, Deserialize)]
pub enum ToolCallStatus {
    Running,
    Success,
    Failed(String),
    /// 被权限策略或用户拒绝
    Denied(String),
    /// 等待用户审批
    AwaitingApproval {
        reason: String,
    },
    Pending,
}

/// 单次工具调用记录
#[derive(Clone, Serialize, Deserialize)]
pub struct ToolCallRecord {
    pub id: String,
    pub tool_name: String,
    pub args: serde_json::Value,
    pub result: Option<String>,
    pub status: ToolCallStatus,
    /// 审批原因（仅 AwaitingApproval 时填充）
    #[serde(default)]
    pub approval_reason: Option<String>,
}

/// 一条聊天消息
#[derive(Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    pub timestamp: DateTime<Local>,
    /// LLM 的推理/思考内容（渲染为可折叠区域）
    #[serde(default)]
    pub reasoning: String,
    /// 按 LLM 返回顺序的流式片段（用于有序渲染）
    #[serde(default)]
    pub segments: Vec<StreamSegment>,
    /// 预渲染的 content HTML（加载时计算，避免每次渲染重复解析 markdown）
    #[serde(skip)]
    pub content_html: String,
}

/// 流式输出片段 —— 按 LLM 返回顺序排列，Frontend 依此渲染
#[derive(Clone, Serialize, Deserialize)]
pub enum StreamSegment {
    /// 文本片段
    Text(String),
    /// 推理/思考片段
    Reasoning(String),
    /// 工具调用插入点（调用详情内嵌，单一数据源，不再单独维护 tool_calls 列表）
    ToolCall(ToolCallRecord),
}

/// 对话
#[derive(Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub title: String,
    pub messages: Vec<ChatMessage>,
    /// 最后活动时间
    #[serde(default)]
    pub updated_at: DateTime<Local>,
    /// 消息总数（从 DB 查询，序列化忽略）
    #[serde(default)]
    pub message_count: usize,
}

/// 项目
#[derive(Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: DateTime<Local>,
    pub conversations: Vec<Conversation>,
    /// 自定义 indicator 颜色键（""=默认 cyan）
    #[serde(default)]
    pub indicator_color: String,
    /// 项目最近活跃时间（删对话也不丢失）
    #[serde(default)]
    pub last_activity_at: Option<DateTime<Local>>,
}

/// 从消息中提取对话标题（用首条用户消息的前 N 个字符）
pub fn title_from_messages(messages: &[ChatMessage]) -> String {
    for msg in messages {
        if matches!(msg.role, Role::User) {
            let trimmed = msg.content.trim();
            if !trimmed.is_empty() {
                let max_len = 30;
                if trimmed.len() <= max_len {
                    return trimmed.to_string();
                }
                // 按字符边界截断（UTF-8 安全）
                let mut truncated = String::with_capacity(max_len);
                for ch in trimmed.chars() {
                    if truncated.len() + ch.len_utf8() > max_len {
                        break;
                    }
                    truncated.push(ch);
                }
                truncated.push('…');
                return truncated;
            }
        }
    }
    "new conversation".into()
}

/// 计算对话的轮数（user + assistant 消息对数）
pub fn conversation_rounds(messages: &[ChatMessage]) -> usize {
    let user_count = messages
        .iter()
        .filter(|m| matches!(m.role, Role::User))
        .count();
    let assistant_count = messages
        .iter()
        .filter(|m| matches!(m.role, Role::Assistant))
        .count();
    user_count.min(assistant_count)
}

/// 格式化相对时间（如 "3m ago", "2h ago", "1d ago"）
pub fn format_relative_time(dt: &DateTime<Local>) -> String {
    let now = Local::now();
    let diff = *dt - now;
    let secs = -diff.num_seconds();

    if secs < 60 {
        "just now".into()
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else if secs < 2592000 {
        format!("{}d ago", secs / 86400)
    } else {
        dt.format("%Y-%m-%d").to_string()
    }
}

// ── Plan / PlanNode types ────────────────────────────────────────────────────

/// 树节点（创建阶段暂存用，approve 后转为队列）
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct PlanNode {
    pub idx: u8,
    pub description: String,
    #[serde(default)]
    pub children: Vec<PlanNode>,
    #[serde(default)]
    pub status: StepStatus,
}

impl PlanNode {
    /// 将树（节点列表）转为 completion_queue（无 DB，纯内存操作）
    pub fn build_queue(nodes: &[PlanNode], parent_idx: Option<u8>) -> Vec<QueueItem> {
        let mut queue = Vec::new();
        let mut sorted = nodes.to_vec();
        sorted.sort_by_key(|n| n.idx);

        for node in &sorted {
            if node.children.is_empty() {
                queue.push(QueueItem {
                    status: QueueItemStatus::Pending,
                    batch: vec![Entity {
                        db_id: None,
                        idx: node.idx,
                        parent_idx,
                        description: node.description.clone(),
                        step_status: StepStatus::Pending,
                    }],
                });
            } else {
                let mut children_queue = Self::build_queue(&node.children, Some(node.idx));
                queue.append(&mut children_queue);
                if let Some(last) = queue.last_mut() {
                    last.batch.push(Entity {
                        db_id: None,
                        idx: node.idx,
                        parent_idx: None,
                        description: node.description.clone(),
                        step_status: StepStatus::Pending,
                    });
                }
            }
        }
        queue
    }

    /// 将树递归展平为实体列表（审批阶段渲染用，不经过 DB）
    pub fn to_entities(nodes: &[PlanNode], parent_idx: Option<u8>) -> Vec<Entity> {
        let mut result = Vec::new();
        let mut sorted = nodes.to_vec();
        sorted.sort_by_key(|n| n.idx);
        for node in &sorted {
            let pid = if node.children.is_empty() {
                parent_idx
            } else {
                None
            };
            result.push(Entity {
                db_id: None,
                idx: node.idx,
                parent_idx: pid,
                description: node.description.clone(),
                step_status: StepStatus::Pending,
            });
            if !node.children.is_empty() {
                result.append(&mut Self::to_entities(&node.children, Some(node.idx)));
            }
        }
        result
    }
}

/// 队列中的一条完成实体
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct Entity {
    pub db_id: Option<i64>,
    pub idx: u8,
    pub parent_idx: Option<u8>,
    pub description: String,
    #[serde(default)]
    pub step_status: StepStatus,
}

/// 队列项状态
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub enum QueueItemStatus {
    #[default]
    Pending,
    Current,
    Completed,
}

/// 队列项：一次 CompleteStep 涉及的批次
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct QueueItem {
    #[serde(default)]
    pub status: QueueItemStatus,
    pub batch: Vec<Entity>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Default)]
pub struct Plan {
    pub db_plan_id: Option<String>,
    pub goal: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub completion_queue: Vec<QueueItem>,
    #[serde(default)]
    pub status: PlanStatus,
    pub started_at: Option<DateTime<Utc>>,
    /// 连续未完成步骤的轮次计数（≥3 时注入催促提示）
    #[serde(default)]
    pub stall_count: u32,
    /// 创建阶段暂存的树结构，approve 后转为队列并清空
    #[serde(skip_serializing)]
    pub children: Vec<PlanNode>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub enum StepStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Bolcked,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema, Default)]
pub enum PlanStatus {
    #[default]
    NeedApproval,
    InProgress,
    Completed,
}

// ── Token 用量记录 ───────────────────────────────────────────────────────────

/// 默认上下文窗口上限（token 数）
pub const DEFAULT_CONTEXT_WINDOW: u32 = 1_000_000;

/// 单次 LLM 交互的 token 用量，由 stream 末尾的 Usage chunk 填充。
/// 通过 From<llm::Usage> 从 provider 层转换。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TokenUsageRecord {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub reasoning_tokens: u32,
    pub total_tokens: u32,
    pub cache_hit_tokens: u32,
    pub cache_miss_tokens: u32,
}

// ── 与 db 层的枚举互转 ──

use crate::db::metadata::plan::PlanStatus as DbPlanStatus;
use crate::db::metadata::task::TaskStatus as DbTaskStatus;

impl From<PlanStatus> for DbPlanStatus {
    fn from(s: PlanStatus) -> Self {
        match s {
            PlanStatus::NeedApproval => DbPlanStatus::NeedApproval,
            PlanStatus::InProgress => DbPlanStatus::InProgress,
            PlanStatus::Completed => DbPlanStatus::Completed,
        }
    }
}

impl From<DbPlanStatus> for PlanStatus {
    fn from(s: DbPlanStatus) -> Self {
        match s {
            DbPlanStatus::NeedApproval => PlanStatus::NeedApproval,
            DbPlanStatus::InProgress => PlanStatus::InProgress,
            DbPlanStatus::Completed => PlanStatus::Completed,
            DbPlanStatus::Canceled => PlanStatus::InProgress, // 历史数据兼容：Canceled 视为可重新执行
        }
    }
}

impl From<StepStatus> for DbTaskStatus {
    fn from(s: StepStatus) -> Self {
        match s {
            StepStatus::Pending => DbTaskStatus::Pending,
            StepStatus::InProgress => DbTaskStatus::InProgress,
            StepStatus::Completed => DbTaskStatus::Completed,
            StepStatus::Bolcked => DbTaskStatus::Blocked,
            StepStatus::Failed => DbTaskStatus::Failed,
        }
    }
}

impl From<DbTaskStatus> for StepStatus {
    fn from(s: DbTaskStatus) -> Self {
        match s {
            DbTaskStatus::Pending => StepStatus::Pending,
            DbTaskStatus::InProgress => StepStatus::InProgress,
            DbTaskStatus::Completed => StepStatus::Completed,
            DbTaskStatus::Blocked => StepStatus::Bolcked,
            DbTaskStatus::Failed => StepStatus::Failed,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq)]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
}

// ── UI 消息 ──────────────────────────────────────────────────────────────────

/// Agent 内部流式状态，通过 Arc 与 UI 共享
/// 渲染数据只有 segments 一份（工具调用详情内嵌在 StreamSegment::ToolCall 中）。
#[derive(Clone)]
pub struct StreamingState {
    pub segments: Arc<Mutex<Vec<StreamSegment>>>,
}

/// UI 层的消息表示。运行时使用，不持久化。
#[derive(Clone)]
pub enum UiMessage {
    /// 已完成的静态消息
    Static(ChatMessage),
    /// 流式进行中的消息：segments 由 Agent 异步填充，
    /// UI 通过事件流（而非轮询 version）感知刷新。
    Streaming {
        segments: Arc<Mutex<Vec<StreamSegment>>>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flatten_tree(nodes: &[PlanNode], parent_idx: Option<u8>) -> Vec<(Option<u8>, u8, String)> {
        let mut result = Vec::new();
        let mut sorted = nodes.to_vec();
        sorted.sort_by_key(|n| n.idx);
        for node in &sorted {
            let pid = if node.children.is_empty() {
                parent_idx
            } else {
                None
            };
            result.push((pid, node.idx, node.description.clone()));
            if !node.children.is_empty() {
                result.append(&mut flatten_tree(&node.children, Some(node.idx)));
            }
        }
        result.sort_by_key(|(pid, idx, _)| (*pid, *idx));
        result
    }

    fn entities_sorted(queue: &[QueueItem]) -> Vec<(Option<u8>, u8, String)> {
        let mut items: Vec<(Option<u8>, u8, String)> = queue
            .iter()
            .flat_map(|qi| qi.batch.iter())
            .map(|e| (e.parent_idx, e.idx, e.description.clone()))
            .collect();
        items.sort_by_key(|(pid, idx, _)| (*pid, *idx));
        items
    }

    #[test]
    fn test_tree_queue_roundtrip_with_phases() {
        let tree = vec![
            PlanNode {
                idx: 1,
                description: "Phase 1".into(),
                children: vec![
                    PlanNode {
                        idx: 1,
                        description: "Task 1.1".into(),
                        ..Default::default()
                    },
                    PlanNode {
                        idx: 2,
                        description: "Task 1.2".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            PlanNode {
                idx: 2,
                description: "Phase 2".into(),
                children: vec![PlanNode {
                    idx: 1,
                    description: "Task 2.1".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ];
        let original = flatten_tree(&tree, None);
        let queue = PlanNode::build_queue(&tree, None);
        let from_queue = entities_sorted(&queue);
        assert_eq!(original.len(), from_queue.len());
        for (exp, got) in original.iter().zip(from_queue.iter()) {
            assert_eq!(exp, got);
        }
    }

    #[test]
    fn test_tree_queue_roundtrip_pure_tasks() {
        let tree = vec![
            PlanNode {
                idx: 1,
                description: "Task A".into(),
                ..Default::default()
            },
            PlanNode {
                idx: 2,
                description: "Task B".into(),
                ..Default::default()
            },
            PlanNode {
                idx: 3,
                description: "Task C".into(),
                ..Default::default()
            },
        ];
        let original = flatten_tree(&tree, None);
        let queue = PlanNode::build_queue(&tree, None);
        let from_queue = entities_sorted(&queue);
        assert_eq!(original.len(), from_queue.len());
        for (exp, got) in original.iter().zip(from_queue.iter()) {
            assert_eq!(exp, got);
        }
    }

    #[test]
    fn test_queue_phase_appended_to_last_child() {
        let tree = vec![PlanNode {
            idx: 1,
            description: "Phase".into(),
            children: vec![
                PlanNode {
                    idx: 1,
                    description: "Task 1".into(),
                    ..Default::default()
                },
                PlanNode {
                    idx: 2,
                    description: "Task 2".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }];
        let queue = PlanNode::build_queue(&tree, None);
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].batch.len(), 1);
        assert_eq!(queue[0].batch[0].parent_idx, Some(1));
        assert_eq!(queue[1].batch.len(), 2);
        assert_eq!(queue[1].batch[1].parent_idx, None);
        assert_eq!(queue[1].batch[1].idx, 1);
        assert_eq!(queue[1].batch[1].description, "Phase");
    }

    #[test]
    fn test_parse_json_children_not_skipped() {
        // 模拟 LLM 传入的 JSON，验证 children 被正确解析而非被 serde(skip) 丢弃
        let json = serde_json::json!({
            "goal": "test",
            "description": "desc",
            "children": [
                {"idx": 1, "description": "Phase 1", "children": [
                    {"idx": 1, "description": "Task 1"}
                ]}
            ]
        });
        let plan: Plan = serde_json::from_value(json).expect("should parse");
        assert_eq!(plan.children.len(), 1, "should have 1 phase");
        assert_eq!(plan.children[0].idx, 1);
        assert_eq!(
            plan.children[0].children.len(),
            1,
            "phase should have 1 task"
        );
        assert_eq!(plan.children[0].children[0].idx, 1);
        assert_eq!(plan.children[0].children[0].description, "Task 1");
    }

    #[test]
    fn test_parse_json_pure_tasks() {
        // 纯任务模式
        let json = serde_json::json!({
            "goal": "test",
            "children": [
                {"idx": 1, "description": "Task A"},
                {"idx": 2, "description": "Task B"}
            ]
        });
        let plan: Plan = serde_json::from_value(json).expect("should parse");
        assert_eq!(plan.children.len(), 2);
        assert_eq!(plan.children[0].description, "Task A");
        assert_eq!(plan.children[1].description, "Task B");
    }

    #[test]
    fn test_to_entities_equals_queue_entities() {
        // 验证：审批阶段用 to_entities 和 approve 后用队列展平，结果一致
        let tree = vec![
            PlanNode {
                idx: 1,
                description: "Phase 1".into(),
                children: vec![
                    PlanNode {
                        idx: 1,
                        description: "Task 1.1".into(),
                        ..Default::default()
                    },
                    PlanNode {
                        idx: 2,
                        description: "Task 1.2".into(),
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            PlanNode {
                idx: 2,
                description: "Phase 2".into(),
                children: vec![PlanNode {
                    idx: 1,
                    description: "Task 2.1".into(),
                    ..Default::default()
                }],
                ..Default::default()
            },
        ];

        // 审批阶段：to_entities
        let from_tree = PlanNode::to_entities(&tree, None);

        // approve 后：build_queue → 展平
        let queue = PlanNode::build_queue(&tree, None);
        let from_queue: Vec<Entity> = queue
            .iter()
            .flat_map(|qi| qi.batch.iter())
            .cloned()
            .collect();

        // 分别按 (parent_idx, idx) 排序后比较
        let mut sorted_tree = from_tree;
        sorted_tree.sort_by_key(|e| (e.parent_idx, e.idx));

        let mut sorted_queue = from_queue;
        sorted_queue.sort_by_key(|e| (e.parent_idx, e.idx));

        assert_eq!(
            sorted_tree.len(),
            sorted_queue.len(),
            "entity count: tree={} queue={}",
            sorted_tree.len(),
            sorted_queue.len()
        );
        for (i, (t, q)) in sorted_tree.iter().zip(sorted_queue.iter()).enumerate() {
            assert_eq!(t.parent_idx, q.parent_idx, "row {i}: parent_idx");
            assert_eq!(t.idx, q.idx, "row {i}: idx");
            assert_eq!(t.description, q.description, "row {i}: description");
        }
    }

    #[test]
    fn test_to_entities_pure_tasks() {
        let tree = vec![
            PlanNode {
                idx: 1,
                description: "Task A".into(),
                ..Default::default()
            },
            PlanNode {
                idx: 2,
                description: "Task B".into(),
                ..Default::default()
            },
        ];

        let from_tree = PlanNode::to_entities(&tree, None);
        let queue = PlanNode::build_queue(&tree, None);
        let from_queue: Vec<Entity> = queue
            .iter()
            .flat_map(|qi| qi.batch.iter())
            .cloned()
            .collect();

        let mut sorted_tree = from_tree;
        sorted_tree.sort_by_key(|e| (e.parent_idx, e.idx));
        let mut sorted_queue = from_queue;
        sorted_queue.sort_by_key(|e| (e.parent_idx, e.idx));

        assert_eq!(sorted_tree.len(), sorted_queue.len());
        for (t, q) in sorted_tree.iter().zip(sorted_queue.iter()) {
            assert_eq!(t, q);
        }
    }
}
