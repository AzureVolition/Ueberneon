use racpagent::agent::Tool;
use llm::tool::ToolMeta as _;
use racpagent::agent::AgentMode;
use racpagent_macros::ToolMetaImpl;

/// Reads a file from disk with automatic encoding detection.
#[derive(ToolMetaImpl)]
pub struct ReadFile {
    schema: serde_json::Value,
    read_only: bool,
}

fn make_read_file() -> ReadFile {
    ReadFile {
        schema: serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" }
            },
            "required": ["path"]
        }),
        read_only: true,
    }
}

// 手动实现 Tool 以便测试 execute
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

#[tokio::test]
async fn name_matches_struct() {
    let tool = make_read_file();
    assert_eq!(tool.name(), "ReadFile");
}

#[tokio::test]
async fn description_from_doc() {
    let tool = make_read_file();
    assert_eq!(
        tool.description(),
        "Reads a file from disk with automatic encoding detection."
    );
}

#[tokio::test]
async fn read_only_returns_field_value() {
    let tool = make_read_file();
    assert!(tool.read_only());

    let mut tool2 = make_read_file();
    tool2.read_only = false;
    assert!(!tool2.read_only());
}

#[tokio::test]
async fn schema_returns_field_value() {
    let tool = make_read_file();
    let schema = tool.schema();
    assert!(schema.is_object());
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["required"][0], "path");
}

#[tokio::test]
async fn schema_is_independent_per_instance() {
    let tool1 = make_read_file();
    let tool2 = ReadFile {
        schema: serde_json::json!({"type": "string"}),
        read_only: false,
    };
    assert_eq!(tool1.schema()["required"][0], "path");
    assert_eq!(tool2.schema()["type"], "string");
}

#[tokio::test]
async fn execute_returns_ok() {
    let tool = make_read_file();
    let ctx = racpagent::agent::ToolContext {
        call_id: "test".into(),
        plan_mode: false,
            agent_mode: AgentMode::Ask,
        progress: None,
    };
    let args = serde_json::json!({});
    let result = tool.execute(&ctx, &args).await;
    assert!(result.is_ok());
    assert_eq!(result.unwrap().output, "executed");
}
