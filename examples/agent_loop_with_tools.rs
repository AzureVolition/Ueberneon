
use std::sync::Arc;
use std::time::Duration;

use llm::{
    Chunk, Message, OpenAiProvider, Provider, Request, Role, ToolCall,
};
use llm::tool::ToolContext;
use futures::StreamExt;
use racpagent::tools::{Bash, BashOutput, EditFile, KillShell, JobManager, MultiEdit, Registry, SandboxSpec, WriteFile};
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

    let job_manager = Arc::new(JobManager::new());

    let work_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());

    // 沙箱：默认基于工作目录创建沙箱配置
    let sandbox = SandboxSpec::defaults(&work_dir);

    registry.add(Box::new(Bash::new(
        work_dir,
        Duration::from_secs(120),
        job_manager.clone(),
        sandbox,
    )));

    registry.add(Box::new(BashOutput::new(job_manager.clone())));
    registry.add(Box::new(KillShell::new(job_manager)));

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
                    "你当前所在的文件系统路径是什么,这个文件夹有什么文件".into(),
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
                        println!("tool call error: {err}");
                        format!("error: {err}")
                    } else {
                        println!("tool call success: {}", &result.output);
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
