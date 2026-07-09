use anyhow::{Context, anyhow};
use async_openai::{
    Client,
    types::chat::{
        ChatCompletionRequestSystemMessageArgs, ChatCompletionRequestUserMessageArgs,
        CreateChatCompletionRequestArgs, ResponseFormat,
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

    let schema = schemars::schema_for!(ActionPlan);
    let schema_json = serde_json::to_string_pretty(&schema).unwrap();


    message.push(
        ChatCompletionRequestUserMessageArgs::default()
            .content(format_json_prompt(prompt, &schema_json))
            .build()
            .context("failed to build user message")?
            .into(),
    );
    let format_setting = ResponseFormat::JsonObject;
    
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

fn format_json_prompt(prompt: &str, schema_str: &str) -> String {
    format! (r#"
    {prompt} 
    Analyze the user's request and respond with a JSON object.
    The output must be valid JSON that strictly conforms to this JSON Schema:  
    {schema_str}
     Rules:
    - Output ONLY the raw JSON object, no markdown fences, no explanation
    - All required fields must be present
    - difficulty must be exactly one of: "Easy", "Medium", "Hard"
    - steps' must be a non-empty array
    - Respond with JSON only"#)
}