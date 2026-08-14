// search_book 工具 —— 按关键词搜索整本书，返回「第N页: 片段」。
// 只读工具，供书旁对话按需检索书内内容，避免预灌全书。

use std::path::PathBuf;

use crate::agent::{GenericsTool, ToolContext, ToolResult};
use crate::permission::Decision;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::tools::internal::read_book::ReadBook;
use schemars::JsonSchema;
use serde::Deserialize;
use ueberneon_macros::ToolMetaImpl;

/// 默认最大结果条数。
const SEARCH_DEFAULT_MAX_RESULTS: usize = 40;
/// 默认最大字符数。
const SEARCH_DEFAULT_MAX_CHARS: usize = 6000;

/// search_book —— 按关键词搜索全书并返回页码与片段。
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(argType = SearchBookParams)]
pub struct SearchBook;

/// search_book 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchBookParams {
    /// 书名或书 ID。
    #[schemars(description = "Book name or book id")]
    pub book: String,
    /// 搜索关键词。
    #[schemars(description = "Keyword to search across the book")]
    pub query: String,
    /// 最大返回条数（默认 40）。
    #[serde(default = "default_max_results")]
    #[schemars(range(min = 1, max = 100), description = "Max result lines to return (default 40)")]
    pub max_results: usize,
    /// 返回内容上限（字符数，默认 6000）。
    #[serde(default = "default_max_chars")]
    #[schemars(range(min = 200, max = 20000), description = "Max characters to return (default 6000)")]
    pub max_chars: usize,
}

fn default_max_results() -> usize {
    SEARCH_DEFAULT_MAX_RESULTS
}

fn default_max_chars() -> usize {
    SEARCH_DEFAULT_MAX_CHARS
}

impl SearchBook {
    pub fn new() -> Self {
        Self
    }

    async fn do_execute(
        &self,
        _ctx: &ToolContext,
        args: &SearchBookParams,
    ) -> Result<ToolResult, String> {
        let query = args.query.trim();
        if query.is_empty() {
            return Err("search_book: query 不能为空".to_string());
        }
        let book = ReadBook::resolve_book(&args.book)?;
        let text = ReadBook::search_pages(&PathBuf::from(&book.path), query, args.max_chars.max(200));
        if text == "未找到匹配内容" {
            return Ok(ToolResult::ok(text));
        }
        let lines: Vec<&str> = text.lines().take(args.max_results.max(1)).collect();
        if lines.is_empty() {
            return Ok(ToolResult::ok("未找到匹配内容"));
        }
        Ok(ToolResult::ok(lines.join("\n")))
    }
}

impl Default for SearchBook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl GenericsTool for SearchBook {
    async fn generics_execute(
        &self,
        ctx: &ToolContext,
        args: &SearchBookParams,
    ) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for SearchBook {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::tool::ToolMeta;

    #[test]
    fn schema_has_required_fields() {
        let schema = SearchBook::new().schema();
        let obj = schema.as_object().expect("schema object");
        let props = obj["properties"].as_object().expect("properties");
        for field in ["book", "query", "max_results", "max_chars"] {
            assert!(props.contains_key(field), "缺少字段 {field}");
        }
    }
}
