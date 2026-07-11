use anyhow::Context;
use tracing::Level;
use tracing_subscriber::FmtSubscriber;


use racpagent::tools::internal::read_file::ReadFile;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().context("failed to load .env file")?;

    let subscriber = FmtSubscriber::builder() // 创建一个构建器（Builder）
        .with_max_level(Level::INFO) // 设置日志级别过滤，只记录 INFO 及以上级别
        .finish(); // 完成构建，生成最终的 Subscriber 实例
    tracing::subscriber::set_global_default(subscriber)?; // 将该 Subscriber 设为全局默认，? 用于错误传播

 
 
    Ok(())
}
