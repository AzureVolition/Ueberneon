// grep_tool_example.rs —— 演示 Grep 工具的 LLM Agent 用法。
//
// 运行方式：cargo run --example grep_tool_example
//
// 需要 .env 文件中配置 OPENAI_API_KEY / OPENAI_BASE_URL。
// LLM 将自主决定何时使用 Grep 工具来搜索代码。

use std::sync::Arc;
use std::time::Duration;

use llm::{
    Chunk, Message, OpenAiProvider, Provider, Request, Role, ToolCall,
};
use llm::tool::{AgentMode, ToolContext};
use futures::StreamExt;
use racpagent::tools::{
    Bash, Grep, Registry,
};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 从 .env 或环境变量加载 API 配置
    dotenvy::dotenv().ok();
    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com".into());
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .expect("请在 .env 中设置 OPENAI_API_KEY 或 DEEPSEEK_API_KEY");
    let model = std::env::var("OPENAI_MODEL")
        .unwrap_or_else(|_| "deepseek-v4-flash".into());

    let provider = OpenAiProvider::new(
        "deepseek".into(),
        base_url,
        model,
        api_key,
        Some("high".into()),
        false,
        None,
    )?;

    let registry = Registry::new();
    let work_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());

    // 注册搜索和执行工具
    registry.add(Box::new(Grep::new()));
    registry.add(Box::new(Bash::new(
        work_dir,
        Duration::from_secs(120),
        Arc::new(racpagent::tools::JobManager::new()),
        None,
        vec![],
    )));

    let mut req = Request {
        messages: vec![
            Message {
                role: Role::System,
                content: Some(
                    "你是一个代码搜索助手。你可以使用以下工具：\n\
                    - Grep：在文件或目录中搜索正则表达式，返回 path:line:text 格式的匹配\n\
                    - Bash：执行 shell 命令（如编译、运行测试等）\n\n\
                    当用户询问代码中的信息时，先用 Grep 搜索相关模式，\
                    然后根据结果用 Bash 做进一步验证（如编译检查）。\
                    搜索时可以用正则表达式精确定位代码。"
                        .into(),
                ),
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: Some(
                    "请帮我搜索以下信息：\n\
                    1. 在 src/ 目录中搜索所有 pub fn 开头的函数声明\n\
                    2. 统计一下总共有多少个这样的函数\n\
                    3. 然后告诉我哪些是公开的工具函数（pub mod 或 pub use 导出的）"
                        .into(),
                ),
                ..Default::default()
            },
        ],
        tools: registry.schemas(),
        temperature: 0.3,
        max_tokens: 8192,
    };

    loop {
        let mut have_tool_calls = false;
        let mut stream = provider.stream(&req).await?;

        let mut output = String::new();
        let mut reasoning_content = String::new();
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

        // 先 push assistant 消息
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

        // 执行工具
        for tool in &pending_tool_calls {
            if let Some(t) = registry.get(&tool.name) {
                let ctx = ToolContext {
                    call_id: tool.id.clone(),
                    plan_mode: false,
                    agent_mode: AgentMode::Ask,
                    progress: None,
                };
                let args: serde_json::Value =
                    serde_json::from_str(&tool.arguments).unwrap_or_default();
                let result = t.execute(&ctx, &args).await;

                req.messages.push(Message {
                    role: Role::Tool,
                    content: Some(if let Some(ref err) = result.error() {
                        println!("tool call error: {err}");
                        format!("error: {err}")
                    } else {
                        println!("tool call success: {} lines", result.output().lines().count());
                        result.output().to_string()
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
