use std::sync::{Arc, Mutex};
use racpagent::agent::Tool;
use llm::tool::ToolMeta as _;
use racpagent::agent::{AgentMode, ActionMode};
use racpagent_macros::ToolMetaImpl;

// ── 只读工具，带完整 schema ──

/// Reads a file from disk with automatic encoding detection.
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(schema = r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}"#)]
pub struct ReadFile {}

#[::async_trait::async_trait]
impl racpagent::agent::Tool for ReadFile {
    async fn execute(
        &self,
        _ctx: &racpagent::agent::ToolContext,
        _args: &serde_json::Value,
    ) -> Result<racpagent::agent::ToolResult, String> {
        Ok(racpagent::agent::ToolResult::ok("executed"))
    }
}

// ── 写工具，无额外属性 ──

/// Creates or overwrites a file.
#[derive(ToolMetaImpl)]
pub struct WriteTool {}

// ── 基础测试 ──

#[test]
fn name_matches_struct() {
    let tool = ReadFile {};
    assert_eq!(tool.name(), "ReadFile");
}

#[test]
fn description_from_doc() {
    let tool = ReadFile {};
    assert_eq!(
        tool.description(),
        "Reads a file from disk with automatic encoding detection."
    );
}

#[test]
fn read_only_attr_makes_read_only_true() {
    let tool = ReadFile {};
    assert!(tool.read_only());
}

#[test]
fn no_read_only_attr_defaults_to_false() {
    let tool = WriteTool {};
    assert!(!tool.read_only());
}

#[test]
fn schema_from_attr() {
    let tool = ReadFile {};
    let schema = tool.schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"][0], "path");
    assert_eq!(schema["properties"]["path"]["type"], "string");
}

#[test]
fn no_schema_attr_defaults_to_empty_object() {
    let tool = WriteTool {};
    let schema = tool.schema();
    assert!(schema.is_object());
    assert!(schema.as_object().unwrap().is_empty());
}

#[tokio::test]
async fn execute_returns_ok() {
    let tool = ReadFile {};
    let ctx = racpagent::agent::ToolContext {
        call_id: "test".into(),
        plan_mode: ActionMode::Regular,
        handler: racpagent::agent::AgentHandler { agent_mode: Arc::new(Mutex::new(AgentMode::Ask)), current_plan: Arc::new(Mutex::new(None)) },
        progress: None,
        main_conversation_id: "".into(),
        project_id: None,
    };
    let args = serde_json::json!({});
    let result = tool.execute(&ctx, &args).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().output, "executed");
}
