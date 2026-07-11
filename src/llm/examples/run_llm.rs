use llm::{OpenAiProvider, Provider, Request, Message, Role, Chunk};
use futures::StreamExt;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let provider = OpenAiProvider::new(
        "deepseek".into(),
        "https://api.deepseek.com".into(),
        "deepseek-v4-flash".into(),
        "sk-205a8f2239dd45249ffbc3ccd2e86aca".to_string(),
        Some("high".into()),  // reasoning effort
        false,                // vision
        None,
    )?;

    let req = Request {
        messages: vec![
            Message {
                role: Role::System,
                content: Some("You are a helpful assistant.".into()),
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: Some("Hello!".into()),
                ..Default::default()
            },
        ],
        tools: vec![],       // 或从 tool::Registry::schemas() 获取
        temperature: 0.7,
        max_tokens: 4096,
    };

    let mut stream = provider.stream(&req).await?;

    while let Some(result) = stream.next().await {
        match result {
            Ok(Chunk::Text(t))      => print!("{t}"),
            //Ok(Chunk::Reasoning { text, .. }) => {} // 不打印 thinking
            Ok(Chunk::Usage(u))     => eprintln!("\n[tokens: {}/{}]", u.prompt_tokens, u.completion_tokens),
            Err(e)     => eprintln!("\n[error: {e}]"),
            _ => {}
        }
    }

    Ok(())
}