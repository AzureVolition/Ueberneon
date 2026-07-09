use anyhow::Context;
use futures::StreamExt;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;
use racpagent::llm_test;
use backon::{ExponentialBuilder, Retryable};



#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().context("failed to load .env file")?;

    let subscriber = FmtSubscriber::builder() // 创建一个构建器（Builder）
        .with_max_level(Level::INFO) // 设置日志级别过滤，只记录 INFO 及以上级别
        .finish(); // 完成构建，生成最终的 Subscriber 实例
    tracing::subscriber::set_global_default(subscriber)?; // 将该 Subscriber 设为全局默认，? 用于错误传播
    
    println!("{}", complete_stream_retry(
        "deepseek-v4-flash",
        Some("你是个助手bot"),
        "我想学制作agent,如何计划",
    ).await?);
    Ok(())
}


async fn complete_stream_retry(model: &str, prompt: Option<&str>, input: &str) -> anyhow::Result<String> {
    let op = || async {
        let stream = llm_test::complete_stream(
            model,
            prompt,
            input,
        )
        .await;
    
        let mut output = String::new();
        futures::pin_mut!(stream);
        while let Some(content) = stream.next().await {
            match content {
                Ok(txt) => {
                    output.push_str(&txt);
                    print!("{}", txt);
                },
                Err(e) => {tracing::error!("\nfailed to read stream: {}", e)}
            }
        }
        Ok(output)
    };
    op.retry(ExponentialBuilder::default().with_max_times(3)).await
}