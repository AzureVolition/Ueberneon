// task.rs —— Task 工具，将任务委派给子 Agent 执行。
//
// 主 Agent 通过此工具将子任务指派给数据库中的 SubAgent 配置。
// 子 Agent 在独立的 conversation 中运行，完成后返回结果文本。
//
// 手动实现 ToolMeta（不用 derive 宏），因为 schema 需要动态查询
// 数据库中可用的 SubAgent 列表。

use crate::agent::manager::AgentManager;
use crate::agent::{Tool, ToolContext, ToolResult};
use crate::permission::Decision;
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::common::checkable_tool::CheckableTool;

/// Task 工具：将任务委派给指定的子 Agent。
///
/// 从 agent_configs 表查询 agent_type='SubAgent' 的配置，
/// 动态构建 subagent_name 的 enum schema。
pub struct Task;

impl Task {
    pub fn new() -> Self {
        Self
    }

    /// 动态构建 JSON Schema，从 DB 查询所有 SubAgent。
    fn build_schema() -> serde_json::Value {
        let subagents: Vec<(String, String)> = crate::db::with_db(|conn| {
            crate::db::metadata::agent_config::list_by_type(conn, "SubAgent")
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.name.clone(), r.description.clone()))
                .collect()
        });

        let enum_values: Vec<Value> = subagents
            .iter()
            .map(|(name, desc)| {
                json!({ "value": name, "description": desc })
            })
            .collect();

        let enum_desc = if subagents.is_empty() {
            "可用的子 Agent 名称（当前无可用子 Agent）".to_string()
        } else {
            let names: Vec<&str> = subagents.iter().map(|(n, _)| n.as_str()).collect();
            format!("可用的子 Agent 名称: {}", names.join(", "))
        };

        json!({
            "type": "object",
            "required": ["subagent_name", "prompt"],
            "properties": {
                "subagent_name": {
                    "type": "string",
                    "enum": enum_values,
                    "description": enum_desc
                },
                "prompt": {
                    "type": "string",
                    "description": "委派给子 Agent 的任务描述"
                }
            }
        })
    }
}

// ── ToolMeta 手动实现 ──

impl llm::tool::ToolMeta for Task {
    fn name(&self) -> &str {
        "Task"
    }

    fn description(&self) -> &str {
        "将任务委派给指定的子 Agent 执行，子 Agent 在独立对话中运行并返回结果"
    }

    fn schema(&self) -> serde_json::Value {
        Self::build_schema()
    }

    fn read_only(&self) -> bool {
        false
    }

    fn schema_str_str(&self) -> &str {
        ""
    }
}

// ── 编译时注册 InternalToolMeta ──

#[cfg(not(test))]
::inventory::submit! {
    crate::tools::InternalToolMeta {
        name: "Task",
        description: "将任务委派给指定的子 Agent 执行，子 Agent 在独立对话中运行并返回结果",
        read_only: false,
        schema: "",
    }
}

// ── Tool 实现 ──

#[async_trait]
impl Tool for Task {
    async fn execute(&self, ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let subagent_name = args
            .get("subagent_name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'subagent_name'".to_string())?;

        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .ok_or_else(|| "missing 'prompt'".to_string())?;

        // 1. 从 DB 查询子 Agent 配置
        let row = crate::db::with_db_result(|conn| {
            crate::db::metadata::agent_config::get_by_name(conn, subagent_name)
        })
        .map_err(|e| format!("db error: {e}"))?
        .ok_or_else(|| format!("子 Agent '{}' 未找到", subagent_name))?;

        // 2. 获取主 Agent 的 conversation_id 作为 parent
        let parent_id = ctx.main_conversation_id.clone();

        // 3. 通过 AgentManager 创建子 Agent
        let mgr = AgentManager::get();
        let (sub_conv_id, _handler) = mgr
            .init_or_get(
                None,                              // 新建对话
                ctx.project_id.clone(),
                Some(&row.id),
                Some(&parent_id),                  // 设置父对话
            )
            .map_err(|e| format!("创建子 Agent 失败: {e}"))?;

        // 4. 取出子 Agent 所有权并运行
        let mut sub_agent = mgr
            .remove(&sub_conv_id)
            .ok_or_else(|| "子 Agent 未找到".to_string())?;

        sub_agent.create_streaming();
        let cancel_token = CancellationToken::new();

        let result = sub_agent
            .accept_message(prompt.to_string(), cancel_token)
            .await;

        // 5. 提取输出
        let output = match result {
            Ok(ui_msg) => match ui_msg {
                crate::model::UiMessage::Static(msg) => msg.content,
                crate::model::UiMessage::Streaming { .. } => "streaming not expected".to_string(),
            },
            Err(e) => format!("子 Agent 执行失败: {e}"),
        };

        // 子 Agent 执行完毕，不注册回缓存（清理）
        drop(sub_agent);

        Ok(ToolResult::ok(output))
    }
}

// ── CheckableTool 实现 ──

#[async_trait]
impl CheckableTool for Task {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}
