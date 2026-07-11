use llm::{
    Chunk, Message, OpenAiProvider, Provider, Request, Role, ToolCall,
};
use llm::tool::ToolContext;
use racpagent::tools::registry::Registry;
use futures::StreamExt;
use racpagent::tools::internal::read_file::ReadFile;


#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiProvider::new(
        "deepseek".into(),
        "https://api.deepseek.com".into(),
        "deepseek-v4-flash".into(),
        "sk-205a8f2239dd45249ffbc3ccd2e86aca".to_string(),
        Some("high".into()),
        false,
        None,
    )?;

    let registry = Registry::new();
    registry.add(Box::new(ReadFile::new()));

    let mut req = Request {
        messages: vec![
            Message {
                role: Role::System,
                content: Some("You are a helpful assistant.".into()),
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: Some(
                    "/Users/linjiageng/code/rust/racpagent/Cargo.toml 里面是什么内容".into(),
                ),
                ..Default::default()
            },
        ],
        tools: registry.schemas(),
        temperature: 0.7,
        max_tokens: 4096,
    };

    loop {
        let mut have_tool_calls = false;
        let mut stream = provider.stream(&req).await?;

        let mut output = String::new();
        let mut reasoning_content = String::new();
        // 先收完整个 stream，不急着执行工具
        let mut pending_tool_calls: Vec<ToolCall> = Vec::new();

        while let Some(result) = stream.next().await {
            match &result {
                Ok(Chunk::Text(t)) => {
                    output.push_str(t);
                    print!("{t}");
                }
                Ok(Chunk::Reasoning { text, .. }) => {
                    reasoning_content.push_str(text);
                }
                Ok(Chunk::Usage(u)) => {
                    eprintln!(
                        "\n[tokens: {} prompt | {} completion]",
                        u.prompt_tokens, u.completion_tokens
                    );
                }
                Ok(Chunk::ToolCallComplete(tool)) => {
                    eprintln!("\n[tool call: {} — {}]", tool.name, tool.arguments);
                    have_tool_calls = true;
                    pending_tool_calls.push(tool.clone());
                }
                Err(e) => eprintln!("\n[error: {e}]"),
                _ => {}
            }
        }

        // 先 push assistant 消息（带上 tool_calls，这是 API 协议的硬要求）
        {
            let mut msg = Message {
                role: Role::Assistant,
                content: Some(output),
                reasoning_content: Some(reasoning_content),
                ..Default::default()
            };
            if !pending_tool_calls.is_empty() {
                msg.tool_calls = pending_tool_calls.clone();
            }
            req.messages.push(msg);
        }

        // 再执行工具，结果作为 Tool 角色消息推入
        for tool in &pending_tool_calls {
            if let Some(t) = registry.get(&tool.name) {
                let ctx = ToolContext {
                    call_id: tool.id.clone(),
                    plan_mode: false,
                    progress: None,
                };
                let args: serde_json::Value =
                    serde_json::from_str(&tool.arguments).unwrap_or_default();
                let result = t.execute(&ctx, &args).await;

                req.messages.push(Message {
                    role: Role::Tool,
                    content: Some(if let Some(ref err) = result.error {
                        format!("error: {err}")
                    } else {
                        result.output
                    }),
                    tool_call_id: Some(tool.id.clone()),
                    name: Some(tool.name.clone()),
                    ..Default::default()
                });
            } else {
                eprintln!("\n[error: tool '{}' not found in registry]", tool.name);
            }
        }

        if !have_tool_calls {
            break;
        }
    }

    Ok(())
}
