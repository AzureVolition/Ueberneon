use std::time::Duration;
use backon::{ExponentialBuilder, Retryable};
use reqwest::StatusCode;
use crate::provider::ProviderError;

const MAX_RETRIES: usize = 10;
const BASE_DELAY: Duration = Duration::from_millis(500);
const MAX_DELAY: Duration = Duration::from_secs(15);

pub async fn send_with_retry(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> Result<reqwest::Response, ProviderError> {
    let url = format!("{}", base_url);
    let auth_header = format!("Bearer {}", api_key);

    let backoff = ExponentialBuilder::default()
        .with_factor(2.0)
        .with_max_times(MAX_RETRIES)
        .with_min_delay(BASE_DELAY)
        .with_max_delay(MAX_DELAY)
        .with_jitter();

    let fetch = || async {
        let result = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", &auth_header)
            .header("Accept", "text/event-stream")
            .json(body)
            .send()
            .await;

        match result {
            // 成功
            Ok(resp) if resp.status().is_success() => Ok(resp),

            // 可重试的状态码 → 必须返回 Err 才能触发重试
            Ok(resp) if is_retryable(resp.status()) => {
                let status = resp.status().as_u16();
                let _body = resp.text().await; // 消费 body，释放连接
                Err(ProviderError::HttpStatus(status))
            }

            // 不可重试的状态码
            Ok(resp) => {
                let status = resp.status().as_u16();
                Err(ProviderError::HttpStatus(status))
            }

            // 连接错误 → 可重试
            Err(e) if is_conn_reset(&e) => {
                Err(ProviderError::Network(e))
            }

            // 其他网络错误
            Err(e) => Err(ProviderError::Network(e)),
        }
    };

    fetch
        .retry(backoff)
        .when(|e| matches!(e, ProviderError::HttpStatus(s) if is_retryable_status(s)))
        .when(|e| matches!(e, ProviderError::Network(_)))
        .notify(|err, dur| {
            tracing::warn!(target: "llm", error = %err, delay_ms = dur.as_millis(), "llm retry");
        })
        .await
}

fn is_retryable(status: StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500..=599)
}

fn is_retryable_status(code: &u16) -> bool {
    matches!(code, 408 | 429 | 500..=599)
}

fn is_conn_reset(e: &reqwest::Error) -> bool {
    e.is_connect() || e.is_timeout()
}