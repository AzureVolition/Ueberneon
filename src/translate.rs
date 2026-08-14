// ── 阅读器选区翻译 ──
//
// 翻译输入是文本：正文按阅读顺序抽取，公式片段由阅读器替换成 [公式N]
// 占位符；译文返回后由 reinsert_formulas 原样回填公式文本，模型不直接
// 处理公式。本地/远程 OpenAI 兼容服务（含自定义 provider）走
// /v1/chat/completions，复用现有 llm::OpenAiProvider，不新增依赖。

use llm::Provider;

/// v1 固定目标语言。
pub const TARGET_LANGUAGE: &str = "简体中文";

/// 内置翻译子代理的固定 ID：阅读器翻译唯一使用的 agent，
/// 无需用户选择，直接在 Sub Agents 页配置它的 provider 和模型。
pub const TRANSLATE_AGENT_ID: &str = "acfg-builtin-translate";

/// 占位符规则：翻译管线的固定不变量，不随 agent prompt 变化。
const PLACEHOLDER_RULE: &str =
    "文本中的 [公式1]、[公式2] 等占位符代表数学公式，必须原样保留，不要翻译、不要改动占位符本身。";

/// 文档上下文规则：阅读器会在请求尾部追加 `[Document: 书名 | Type: pdf]`，
/// 模型必须用它消歧术语但不能把它翻进译文（与 OakReader 的 system 提示一致）。
const DOC_CONTEXT_RULE: &str =
    "如果提供了文档上下文（方括号内），用它来消歧术语，但不要在输出中包含它。";

/// 用户消息里的翻译指令：Hy-MT2 官方推荐把指令放在 user 消息里，
/// 只把原文放在 user 消息里会让它把“翻译助手”身份当成用户输入，
/// 稳定回“请提供需要翻译的文本”。文档上下文规则用于防止模型把
/// 阅读器追加的 `[Document: ... | Type: pdf]` 也翻译进译文。
const USER_INSTRUCTION: &str = "请将以下文本翻译为简体中文；文本中的 [公式1]、[公式2] 等占位符代表数学公式，必须原样保留，不要翻译、不要改动占位符本身；如果提供了文档上下文（方括号内），用它来消歧术语，但不要在输出中包含它。";

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

fn default_system_prompt() -> String {
    format!(
        "把用户提供的文本翻译成{TARGET_LANGUAGE}，保持术语准确、语句通顺。只输出译文，不要解释、不要复述原文。"
    )
}

/// 返回内置翻译子代理的配置行；未配置 provider/模型时返回 None。
/// 模型、系统提示、温度、max_tokens、api_key 都在 agent 行里，
/// 不需要额外的配置结构。
pub fn translate_agent() -> Option<crate::db::metadata::agent_config::AgentConfigRow> {
    crate::db::with_db(|conn| {
        let agent = crate::db::metadata::agent_config::get(conn, TRANSLATE_AGENT_ID)
            .ok()
            .flatten()?;
        if agent.agent_type != "SubAgent"
            || !crate::db::metadata::agent_config::subagent_effectively_configured(&agent)
        {
            return None;
        }
        Some(agent)
    })
}

/// 把当前书名/文档信息追加到翻译输入末尾，格式与 OakReader 一致：
/// `[Document: 书名 | Type: pdf]`。实测 Hy-MT2 7B 只有看到这个
/// 方括号文档上下文才会把长段落当翻译任务（否则直接输出结束 token）。
pub fn document_context(title: &str) -> String {
    let title = title.trim();
    if title.is_empty() {
        return String::new();
    }
    format!("\n\n[Document: {title} | Type: pdf]")
}

/// 把文档上下文拼到翻译输入末尾（保持单块请求时也能用）。
pub fn with_document_context(source: &str, title: &str) -> String {
    format!("{source}{}", document_context(title))
}

/// 构造翻译请求消息：指令永远放在 user 消息里（Hy-MT2 官方推荐），
/// system 保留 agent 提示（Hy-MT2 需要配合文档上下文规则才能稳定翻译长文本）。
pub fn build_messages(source: &str, system_prompt: &str) -> Vec<llm::Message> {
    let user = llm::Message {
        role: llm::Role::User,
        content: Some(format!("{USER_INSTRUCTION}\n\n{source}")),
        ..Default::default()
    };

    let base = if system_prompt.trim().is_empty() {
        default_system_prompt()
    } else {
        system_prompt.trim().to_string()
    };
    let system = format!("{base}\n\n{PLACEHOLDER_RULE}\n\n{DOC_CONTEXT_RULE}");
    vec![
        llm::Message {
            role: llm::Role::System,
            content: Some(system),
            ..Default::default()
        },
        user,
    ]
}

/// 统一收尾：清洗包装后直接返回模型输出；空结果报错。
pub fn finalize_translation(raw: &str) -> Result<String, String> {
    let cleaned = clean_translation(raw);
    if cleaned.is_empty() {
        return Err("翻译结果为空".to_string());
    }
    Ok(cleaned)
}

/// 把文本按句子拆分：句读（`. ! ? 。！？ ; ；`）在括号深度 0 处生效，
/// 公式占位符 `[公式N]` 内部的符号不会误断句。
pub fn split_sentences(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut depth = 0usize;
    for c in text.chars() {
        cur.push(c);
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            ';' | '；' | '.' | '!' | '?' | '。' | '！' | '？' if depth == 0 => {
                let s = cur.trim();
                if !s.is_empty() {
                    out.push(s.to_string());
                }
                cur.clear();
            }
            _ => {}
        }
    }
    let s = cur.trim();
    if !s.is_empty() {
        out.push(s.to_string());
    }
    out
}

/// 原文句子 → 译文句子区间（含端点）的比例映射：
/// 句数相同一一对应；译文句更多时按比例分摊；译文句更少时多个原文句
/// 共享同一条译文（空组指向边界句，避免悬停死区）。
pub fn align_sentences(source: &[String], translated: &[String]) -> Vec<(usize, usize)> {
    let m = source.len();
    let n = translated.len();
    if m == 0 || n == 0 {
        return Vec::new();
    }
    let mut groups = Vec::with_capacity(m);
    for i in 0..m {
        let start = ((i * n) as f64 / m as f64).round() as usize;
        let end = (((i + 1) * n) as f64 / m as f64).round() as usize;
        if start < end {
            groups.push((start, end - 1));
        } else {
            let idx = start.min(n - 1);
            groups.push((idx, idx));
        }
    }
    groups
}

/// 译文句 j → 对应的原文句索引（多组共享时取第一个）。
pub fn translation_source_index(groups: &[(usize, usize)], j: usize) -> Option<usize> {
    groups.iter().position(|&(s, e)| j >= s && j <= e)
}

/// 把句子按完整边界分块，每块不超过 max_chars（字符数）。
pub fn chunk_sentences(sentences: &[String], max_chars: usize) -> Vec<Vec<String>> {
    let mut chunks: Vec<Vec<String>> = Vec::new();
    let mut cur: Vec<String> = Vec::new();
    let mut cur_len = 0usize;
    for s in sentences {
        let slen = s.chars().count();
        if !cur.is_empty() && cur_len + slen > max_chars {
            chunks.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        cur.push(s.clone());
        cur_len += slen;
    }
    if !cur.is_empty() {
        chunks.push(cur);
    }
    chunks
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
    agent: &crate::db::metadata::agent_config::AgentConfigRow,
    source: &str,
) -> Result<llm::provider::ChunkStream, String> {
    // 与完整 Agent 共用同一份配置解析(SubAgent 实时从实例解析 base_url/api_key)。
    let cfg = crate::agent::manager::AgentManager::read_agent_config(&agent.id)
        .map_err(|e| format!("{e}"))?;
    let provider = llm::OpenAiProvider::new(
        "translation".to_string(),
        cfg.base_url,
        cfg.model,
        cfg.api_key,
        None,
        false,
        None,
    )
    .map_err(|e| format!("{e}"))?;

    let req = llm::Request {
        messages: build_messages(source, &agent.system_prompt),
        tools: Vec::new(),
        temperature: agent.temperature,
        max_tokens: agent.max_tokens.unwrap_or(2048),
    };

    provider.stream(&req).await.map_err(|e| format!("{e}"))
}

/// 发送翻译请求并收集流式输出；Ollama 未启动 / 模型不可用时返回错误文案。
pub async fn translate(
    agent: &crate::db::metadata::agent_config::AgentConfigRow,
    source: &str,
) -> Result<String, String> {
    use futures::StreamExt;
    let mut stream = translation_stream(agent, source).await?;
    let mut text = String::new();
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(llm::Chunk::Text(t)) => text.push_str(&t),
            Ok(_) => {}
            Err(e) => return Err(format!("{e}")),
        }
    }
    finalize_translation(&text)
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
        let user = msgs[1].content.as_deref().unwrap_or_default();
        assert!(user.contains("请将以下文本翻译为简体中文"), "{user}");
        assert!(user.contains("hello [公式1] world"), "{user}");
    }

    #[test]
    fn build_messages_uses_agent_system_prompt_and_appends_rule() {
        let msgs = build_messages("hello", "你是翻译大师，只输出译文。");
        let system = msgs[0].content.as_deref().unwrap_or_default();
        assert!(system.contains("你是翻译大师"), "{system}");
        assert!(system.contains("原样保留"), "{system}");
        assert!(system.contains("文档上下文"), "{system}");
    }

    #[test]
    fn build_messages_keeps_system_for_hy_mt2_and_puts_instruction_in_user() {
        let msgs = build_messages("hello", "你是翻译大师，只输出译文。");
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[1].role, llm::Role::User);
        let user = msgs[1].content.as_deref().unwrap_or_default();
        assert!(user.contains("请将以下文本翻译为简体中文"), "{user}");
        assert!(user.contains("hello"), "{user}");
        assert!(user.contains("文档上下文"), "{user}");
        let system = msgs[0].content.as_deref().unwrap_or_default();
        assert!(system.contains("你是翻译大师"), "{system}");
    }

    #[test]
    fn with_document_context_appends_book_metadata() {
        let out = with_document_context("hello", "Agentic Design Patterns");
        assert_eq!(
            out,
            "hello\n\n[Document: Agentic Design Patterns | Type: pdf]"
        );
    }

    #[test]
    fn with_document_context_skips_empty_title() {
        assert_eq!(with_document_context("hello", ""), "hello");
        assert_eq!(with_document_context("hello", "  "), "hello");
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
    fn finalize_translation_returns_model_output_and_checks_empty() {
        // 模型返回什么就显示什么（只做包装清洗）
        assert_eq!(
            finalize_translation("你好！有什么可以帮你的吗？").unwrap(),
            "你好！有什么可以帮你的吗？"
        );
        assert_eq!(
            finalize_translation("译文：这是正常译文。").unwrap(),
            "这是正常译文。"
        );
        assert!(finalize_translation("").is_err());
        assert!(finalize_translation("\n  \n").is_err());
    }

    #[test]
    fn split_sentences_handles_punctuation_and_formula_placeholders() {
        assert_eq!(
            split_sentences("第一句。第二句！第三句；第四句"),
            vec!["第一句。", "第二句！", "第三句；", "第四句"]
        );
        // 公式占位符内部的分号不误断句
        assert_eq!(
            split_sentences("p0 = plan(E, g; Θ, P); 后面是正文。"),
            vec!["p0 = plan(E, g; Θ, P);", "后面是正文。"]
        );
        assert_eq!(split_sentences("   "), Vec::<String>::new());
    }

    #[test]
    fn align_sentences_maps_proportionally() {
        let src = |n: usize| vec!["s".to_string(); n];
        let tr = |n: usize| vec!["t".to_string(); n];
        // 一一对应
        assert_eq!(
            align_sentences(&src(3), &tr(3)),
            vec![(0, 0), (1, 1), (2, 2)]
        );
        // 译文更多：按比例分摊
        assert_eq!(align_sentences(&src(2), &tr(4)), vec![(0, 1), (2, 3)]);
        // 译文更少：多个原文句共享同一条译文
        let groups = align_sentences(&src(3), &tr(1));
        assert_eq!(groups, vec![(0, 0), (0, 0), (0, 0)]);
        assert_eq!(translation_source_index(&groups, 0), Some(0));
        // 空输入
        assert!(align_sentences(&src(2), &tr(0)).is_empty());
    }

    #[test]
    fn chunk_sentences_respects_max_chars_and_sentence_boundaries() {
        let sentences = vec![
            "第一句。".to_string(),
            "第二句比较长一些。".to_string(),
            "第三句。".to_string(),
        ];
        let chunks = chunk_sentences(&sentences, 14);
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[0], vec!["第一句。", "第二句比较长一些。"]);
        assert_eq!(chunks[1], vec!["第三句。"]);
        // 足够大时只有一块
        assert_eq!(chunk_sentences(&sentences, 100).len(), 1);
        assert!(chunk_sentences(&[], 100).is_empty());
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
