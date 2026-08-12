// ── 模型列表动态刷新 ──
//
// 从 provider 的 GET /models（OpenAI 兼容）端点拉取最新模型列表，
// 并写回 provider_models 表。

use crate::db::metadata::provider::{self, ProviderRow};

/// 向 provider 的 GET /v1/models 发起请求，返回模型 ID 列表
pub async fn fetch_models(provider: &ProviderRow, api_key: &str) -> Result<Vec<String>, String> {
    // 构建候选 URL
    let urls = build_fetch_urls(&provider.base_url, &provider.models_url);

    let client = reqwest::Client::new();
    let mut last_err = String::new();

    for url in &urls {
        match try_fetch(&client, url, api_key).await {
            Ok(models) => return Ok(models),
            Err(e) => last_err = e,
        }
    }

    Err(last_err)
}

/// 从 URL 尝试拉取模型列表
async fn try_fetch(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let mut req = client.get(url).header("Accept", "application/json");
    if !api_key.is_empty() {
        req = req.header("Authorization", format!("Bearer {api_key}"));
    }
    let resp = req
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("HTTP {}", resp.status()));
    }

    #[derive(serde::Deserialize)]
    struct ModelEntry {
        id: String,
    }

    #[derive(serde::Deserialize)]
    struct ModelsResponse {
        data: Vec<ModelEntry>,
    }

    let body: ModelsResponse = resp
        .json()
        .await
        .map_err(|e| format!("decode response: {e}"))?;

    let mut ids: Vec<String> = body.data.into_iter().map(|m| m.id).collect();
    ids.sort();
    Ok(ids)
}

/// 构建候选 fetch URL 列表
fn build_fetch_urls(base_url: &str, models_url: &str) -> Vec<String> {
    if !models_url.is_empty() {
        return vec![models_url.to_string()];
    }

    let base = base_url.trim_end_matches('/');
    let mut candidates = vec![format!("{base}/models")];

    // 如果 base_url 已经以 /v1 结尾，也尝试 /v1/models
    if base.ends_with("/v1") {
        // already covered by {base}/models
    } else {
        candidates.push(format!("{base}/v1/models"));
    }

    candidates
}

/// 刷新并保存：从 API 拉取 → 写入 provider_models 表
pub async fn refresh_and_save(
    conn: &rusqlite::Connection,
    provider: &ProviderRow,
    api_key: &str,
) -> Result<Vec<String>, String> {
    let models = fetch_models(provider, api_key).await?;
    provider::replace_models(conn, &provider.id, &models).map_err(|e| format!("db error: {e}"))?;
    Ok(models)
}
