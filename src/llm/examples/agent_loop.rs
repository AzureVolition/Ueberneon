use llm::{OpenAiProvider, Provider, Request, Message, Role, Chunk};
use futures::StreamExt;

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

 
    let mut req = Request {
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
        tools: vec![],       
        temperature: 0.7,
        max_tokens: 4096,
    };
    
    loop {
        let mut have_tool_calls = false;
        let mut stream = provider.stream(&req).await?;
    
        let mut output = String::new();
        let mut reasoning_content = String::new();
        
        while let Some(chunk) = stream.next().await {
            let mut content: Option<String> = None;
            match &chunk {
                Chunk::Text(t)      => {output.push_str(&t); content = Some(t.clone()); }
                Chunk::Reasoning { text, .. } => {

                    reasoning_content.push_str(&text);
                    // content = Some(text);
                }  
                Chunk::Usage(u)     => eprintln!("\n[tokens: {}/{}]", u.prompt_tokens, u.completion_tokens),
                Chunk::Error(e)     => eprintln!("\n[error: {e}]"),
                Chunk::ToolCallComplete(tool) => {
                    println!("\n[tool call complete: {tool:?}]");
                    have_tool_calls = true;
                }
                _ => {}
            }

            if let Some(c) = content {
                print!("{}", c);
            }
        }
        req.messages.push(Message {
            role: Role::Assistant,
            content: Some(output.into()),
            reasoning_content: Some(reasoning_content.into()),
            ..Default::default()
        });
        if !have_tool_calls {
            break;
        }
    }
    
    Ok(())
}