use anyhow::{Context, anyhow};
use async_openai::{
    Client,
    types::chat::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ResponseFormat, ResponseFormatJsonSchema,
    },
};
use crate::models::ActionPlan;

pub async fn complete_structured(model: &str, system: Option<&str>, prompt: &str) -> anyhow::Result<ActionPlan> {
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

    let schema = schemars::schema_for!(ActionPlan);
    let schema_json = schema.as_value().clone();
    let format_setting = ResponseFormat::JsonSchema { 
        json_schema: ResponseFormatJsonSchema {
            description: Some("A step-by-step agent action plan with diffifulty and time estimate".into()),
            name: "action".into(),
            schema: schema_json,
            strict: Some(true),
        },
    };
    
    let request = CreateChatCompletionRequestArgs::default()
        .model(model)
        .messages(message)
        .response_format(format_setting)
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
        .ok_or_else(|| anyhow!("no response content"))
        .and_then(|s| serde_json::from_str(&s).map_err(Into::into))?;
    Ok(content)
}
