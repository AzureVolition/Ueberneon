use anyhow::{Context};
use async_openai::{
    Client,
    types::chat::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs,
    },
};
use async_stream::stream;
use futures::{Stream, StreamExt};

pub async fn complete_stream(model: &str, system: Option<&str>, prompt: &str) -> impl Stream<Item = anyhow::Result<String>> {
    stream!{
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

    let mut stream = client
        .chat()
        .create_stream(request)
        .await
        .context("failed to complete")?;
    

    while let Some(response_list) = stream.next().await {
        match response_list {
            Ok(chunck) =>{
                if let Some(content) = chunck.choices.first()
                    && let Some(txt) = &content.delta.content{
                    yield Ok(txt.clone())
                }
            },Err(e)=>{
                yield Err(e.into())
            }
        }
    }
}
}
