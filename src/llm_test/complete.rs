use anyhow::{Context, anyhow};
use async_openai::{
    Client,
    types::chat::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs,
    },
};

pub async fn complete(model: &str, system: Option<&str>, prompt: &str) -> anyhow::Result<String> {
    let client = Client::new();
    let mut message = vec![];

    if let Some(system) = system {
        message.push(
            ChatCompletionRequestSystemMessageArgs::default()
                .content(system)
                .build()
                .context("failed to build system message")?
                .into(),
        );
    }
    message.push(
        ChatCompletionRequestUserMessageArgs::default()
            .content(prompt)
            .build()
            .context("failed to build user message")?
            .into(),
    );
    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(message)
        .max_tokens(2048u32)
        .build()
        .context("failed to build request")?;

    let response = client
        .chat()
        .create(request)
        .await
        .context("failed to complete")?;
    // tracing::info!("模型返回: {:#?}", response);

    let content = response
        .choices
        .into_iter()
        .next()
        .and_then(|choice| choice.message.content)
        .ok_or_else(|| anyhow!("no response content"))?;
    Ok(content)
}
