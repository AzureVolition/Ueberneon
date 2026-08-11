// ── 阅读器选区翻译 ──
//
// 翻译输入是文本：正文按阅读顺序抽取，公式片段由阅读器替换成 [公式N]
// 占位符；译文返回后由 reinsert_formulas 原样回填公式文本，模型不直接
// 处理公式。Ollama 本地实例走 OpenAI 兼容 /v1/chat/completions，
// 复用现有 llm::OpenAiProvider，不新增依赖。

use base64::Engine as _;
use llm::Provider;

/// v1 固定目标语言。
pub const TARGET_LANGUAGE: &str = "简体中文";

/// 内置翻译子代理的固定 ID：阅读器翻译唯一使用的 agent，
/// 无需用户选择，直接在 Sub Agents 页配置它的 provider 和模型。
pub const TRANSLATE_AGENT_ID: &str = "acfg-builtin-translate";

/// 占位符规则：翻译管线的固定不变量，不随 agent prompt 变化。
const PLACEHOLDER_RULE: &str =
    "文本中的 [公式1]、[公式2] 等占位符代表数学公式，必须原样保留，不要翻译、不要改动占位符本身。";

/// 模型可能多输出的常见前缀（一次清洗与流式清洗共用）。
const TRANSLATION_PREFIXES: [&str; 13] = [
    "译文：",
    "译文:",
    "翻译：",
    "翻译:",
    "中文翻译：",
    "中文翻译:",
    "以下是译文：",
    "以下是译文:",
    "翻译结果：",
    "翻译结果:",
    "Translation:",
    "Translated:",
    "Chinese translation:",
];

/// 翻译请求所需的 provider 连接信息（api_key 已解码）。
#[derive(Debug, Clone)]
pub struct TranslationConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: String,
    pub system_prompt: String,
    pub temperature: f64,
    pub max_tokens: u32,
}

fn default_system_prompt() -> String {
    format!(
        "你是学术文献翻译助手。请把用户提供的文本翻译成{TARGET_LANGUAGE}，保持术语准确，只输出译文，不要解释。"
    )
}

/// 解析内置翻译子代理的请求配置；未配置 provider/模型时返回 None。
pub fn translation_config() -> Option<TranslationConfig> {
    crate::db::with_db(|conn| {
        let agent = crate::db::metadata::agent_config::get(conn, TRANSLATE_AGENT_ID)
            .ok()
            .flatten()?;
        if agent.agent_type != "SubAgent" || agent.model.is_empty() {
            return None;
        }
        let inst = crate::db::metadata::provider_instance::get(conn, &agent.provider_instance_id)
            .ok()
            .flatten()?;
        let prov = crate::db::metadata::provider::get(conn, &inst.provider_id)
            .ok()
            .flatten()?;
        let key = if inst.api_key.is_empty() {
            String::new()
        } else {
            base64::engine::general_purpose::STANDARD
                .decode(inst.api_key.as_bytes())
                .ok()
                .and_then(|v| String::from_utf8(v).ok())
                .unwrap_or_default()
        };
        Some(TranslationConfig {
            base_url: prov.base_url,
            model: agent.model,
            api_key: key,
            system_prompt: agent.system_prompt,
            temperature: agent.temperature,
            max_tokens: agent.max_tokens.unwrap_or(2048),
        })
    })
}

/// 构造翻译请求消息：agent 的系统提示（为空时用内置默认）+ 占位符规则。
pub fn build_messages(source: &str, system_prompt: &str) -> Vec<llm::Message> {
    let base = if system_prompt.trim().is_empty() {
        default_system_prompt()
    } else {
        system_prompt.trim().to_string()
    };
    let system = format!("{base}\n\n{PLACEHOLDER_RULE}");
    vec![
        llm::Message {
            role: llm::Role::System,
            content: Some(system),
            ..Default::default()
        },
        llm::Message {
            role: llm::Role::User,
            content: Some(source.to_string()),
            ..Default::default()
        },
    ]
}

/// 把 [公式N] 占位符替换回公式原文；模型漏掉的占位符按顺序追加到译文末尾。
pub fn reinsert_formulas(translated: &str, formulas: &[String]) -> String {
    let mut out = translated.to_string();
    let mut missing = Vec::new();
    for (i, formula) in formulas.iter().enumerate() {
        let placeholder = format!("[公式{}]", i + 1);
        if out.contains(&placeholder) {
            out = out.replace(&placeholder, formula);
        } else {
            missing.push(formula.clone());
        }
    }
    if !missing.is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&missing.join("\n"));
    }
    out
}

/// 宽松清洗模型返回：去掉 markdown 围栏、JSON 信封、包裹引号与常见前缀。
/// 只处理明确的包装层，不会改动译文正文。
pub fn clean_translation(raw: &str) -> String {
    let mut t = raw.trim().to_string();
    if t.is_empty() {
        return t;
    }

    // markdown 代码围栏（``` 或 ~~~）
    for fence in ["```", "~~~"] {
        if t.starts_with(fence) {
            if let Some(end) = t.rfind(fence) {
                if end > fence.len() {
                    let content_start = match t.find('\n') {
                        Some(i) if i + 1 <= end => i + 1,
                        _ => fence.len(),
                    };
                    t = t[content_start..end].trim().to_string();
                }
            }
            break;
        }
    }
    if t.is_empty() {
        return t;
    }

    // 宽松 JSON 信封：{"translation": "..."} 或 {"translation":"..."}
    if t.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&t) {
            if let Some(s) = v.get("translation").and_then(|x| x.as_str()) {
                t = s.trim().to_string();
            }
        }
    }
    if t.is_empty() {
        return t;
    }

    // 成对包裹引号
    for pair in ["“”", "「」", "\"\"", "''"] {
        let mut chars = pair.chars();
        let l = chars.next().unwrap();
        let r = chars.next().unwrap();
        if t.starts_with(l) && t.ends_with(r) && t.chars().count() >= 2 {
            t = t[l.len_utf8()..t.len() - r.len_utf8()].trim().to_string();
            break;
        }
    }
    if t.is_empty() {
        return t;
    }

    // 常见前缀
    for prefix in TRANSLATION_PREFIXES {
        if t.starts_with(prefix) {
            t = t[prefix.len()..].trim().to_string();
            break;
        }
    }
    t
}

/// 流式展示用的“稳定清洗”：只处理看开头就能确定的包装
/// （围栏首行、常见前缀、前导空白），避免边显示边被后续内容推翻；
/// JSON 信封与包裹引号需要看到结尾才能确定，仍由结束时的一次性清洗处理。
pub fn stream_visible(raw: &str) -> String {
    let t = raw.trim_start().to_string();
    if t.is_empty() {
        return t;
    }

    // 围栏首行：` ```json` 整行不显示，从正文第一行开始显示
    for fence in ["```", "~~~"] {
        if t.starts_with(fence) {
            return match t.find('\n') {
                Some(i) => t[i + 1..].trim_start().to_string(),
                None => String::new(),
            };
        }
    }

    // 常见前缀：已完整出现且后面还有内容 → 立即剥掉；
    // 只收到前缀的一部分 → 先不显示，等下一块再决定。
    for prefix in TRANSLATION_PREFIXES {
        if t.len() > prefix.len() && t.starts_with(prefix) {
            return t[prefix.len()..].trim_start().to_string();
        }
        if t.len() <= prefix.len() && prefix.starts_with(&t) {
            return String::new();
        }
    }
    t
}

/// 返回翻译流（不收集），调用方逐块消费并自行展示。
/// Ollama 未启动 / 模型不可用时返回错误文案。
pub async fn translation_stream(
    config: &TranslationConfig,
    source: &str,
) -> Result<llm::provider::ChunkStream, String> {
    let provider = llm::OpenAiProvider::new(
        "translation".to_string(),
        config.base_url.clone(),
        config.model.clone(),
        config.api_key.clone(),
        None,
        false,
        None,
    )
    .map_err(|e| format!("{e}"))?;

    let req = llm::Request {
        messages: build_messages(source, &config.system_prompt),
        tools: Vec::new(),
        temperature: config.temperature,
        max_tokens: config.max_tokens,
    };

    provider.stream(&req).await.map_err(|e| format!("{e}"))
}

/// 发送翻译请求并收集流式输出；Ollama 未启动 / 模型不可用时返回错误文案。
pub async fn translate(config: &TranslationConfig, source: &str) -> Result<String, String> {
    use futures::StreamExt;
    let mut stream = translation_stream(config, source).await?;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(llm::Chunk::Text(t)) => text.push_str(&t),
            Ok(_) => {}
            Err(e) => return Err(format!("{e}")),
        }
    }
    let cleaned = clean_translation(&text);
    if cleaned.is_empty() {
        Err("翻译结果为空".to_string())
    } else {
        Ok(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_messages_contains_target_language_and_placeholder_rule() {
        let msgs = build_messages("hello [公式1] world", "");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, llm::Role::System);
        let system = msgs[0].content.as_deref().unwrap_or_default();
        assert!(system.contains(TARGET_LANGUAGE), "{system}");
        assert!(system.contains("[公式1]"), "{system}");
        assert!(system.contains("原样保留"), "{system}");
        assert_eq!(msgs[1].role, llm::Role::User);
        assert_eq!(msgs[1].content.as_deref(), Some("hello [公式1] world"));
    }

    #[test]
    fn build_messages_uses_agent_system_prompt_and_appends_rule() {
        let msgs = build_messages("hello", "你是翻译大师，只输出译文。");
        let system = msgs[0].content.as_deref().unwrap_or_default();
        assert!(system.contains("你是翻译大师"), "{system}");
        assert!(system.contains("原样保留"), "{system}");
    }

    #[test]
    fn reinsert_formulas_replaces_present_placeholders_in_order() {
        let formulas = vec!["p0 = plan(E, g; Θ, P);".to_string(), "x_i^2".to_string()];
        let out = reinsert_formulas("前文 [公式1] 中段 [公式2] 后文", &formulas);
        assert_eq!(out, "前文 p0 = plan(E, g; Θ, P); 中段 x_i^2 后文");
    }

    #[test]
    fn reinsert_formulas_keeps_other_text_untouched() {
        let out = reinsert_formulas("没有占位符的译文", &[]);
        assert_eq!(out, "没有占位符的译文");
    }

    #[test]
    fn reinsert_formulas_appends_missing_placeholders_at_end() {
        let formulas = vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()];
        // 模型只保留了第二个占位符
        let out = reinsert_formulas("只保留 [公式2]", &formulas);
        assert_eq!(out, "只保留 beta\nalpha\ngamma");
    }

    #[test]
    fn clean_translation_keeps_plain_output() {
        assert_eq!(clean_translation("  这是译文。  "), "这是译文。");
    }

    #[test]
    fn clean_translation_strips_markdown_fence() {
        let raw = "```json\n{\"translation\": \"译文内容\"}\n```";
        assert_eq!(clean_translation(raw), "译文内容");
    }

    #[test]
    fn clean_translation_parses_json_envelope() {
        assert_eq!(
            clean_translation(r#"{"translation": "模型返回的译文"}"#),
            "模型返回的译文"
        );
    }

    #[test]
    fn clean_translation_strips_common_prefix_and_quotes() {
        assert_eq!(clean_translation("译文：模型返回的译文"), "模型返回的译文");
        assert_eq!(clean_translation("“模型返回的译文”"), "模型返回的译文");
        assert_eq!(clean_translation("Translation: hello"), "hello");
    }

    #[test]
    fn clean_translation_keeps_unknown_json_raw() {
        // 非 translation 字段的 JSON 不回退成空，保持原样
        let raw = r#"{"foo": "bar"}"#;
        assert_eq!(clean_translation(raw), raw);
    }

    #[test]
    fn stream_visible_keeps_plain_text() {
        assert_eq!(stream_visible("  译文正文"), "译文正文");
    }

    #[test]
    fn stream_visible_strips_completed_prefix_immediately() {
        assert_eq!(stream_visible("译文：模型输出"), "模型输出");
    }

    #[test]
    fn stream_visible_waits_for_partial_prefix() {
        assert_eq!(stream_visible("译"), "");
        assert_eq!(stream_visible("译文"), "");
        assert_eq!(stream_visible("译文："), "");
        assert_eq!(stream_visible("译文：正"), "正");
    }

    #[test]
    fn stream_visible_hides_fence_opening_line() {
        assert_eq!(stream_visible("```json\n{"), "{");
        assert_eq!(stream_visible("```"), "");
    }

    #[test]
    fn stream_visible_does_not_swallow_unknown_leading_text() {
        assert_eq!(stream_visible("译作如下"), "译作如下");
    }
}
