// cite_book 工具 —— 记录回答中的书内引用（页码 + 原文片段）。
// 只读、无副作用；调用记录随消息落库，前端据此渲染可点击引用指针。

use std::path::Path;

use crate::agent::{GenericsTool, ToolContext, ToolResult};
use crate::permission::Decision;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::tools::internal::read_book::ReadBook;
use schemars::JsonSchema;
use serde::Deserialize;
use ueberneon_macros::ToolMetaImpl;

/// cite_book —— 标记一次书内引用，供用户跳回原文。
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(argType = CiteBookParams)]
pub struct CiteBook;

/// cite_book 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
pub struct CiteBookParams {
    /// 书名或书 ID。
    #[schemars(description = "Book name or book id")]
    pub book: String,
    /// 1-based 页码。
    #[schemars(description = "1-based page number being cited")]
    pub page: u32,
    /// 被引用的原文片段（尽量短，用于跳转后高亮）。
    #[schemars(description = "Short original quote used to highlight the citation")]
    pub quote: String,
}

impl CiteBook {
    pub fn new() -> Self {
        Self
    }

    async fn do_execute(
        &self,
        _ctx: &ToolContext,
        args: &CiteBookParams,
    ) -> Result<ToolResult, String> {
        if args.page == 0 {
            return Err("cite_book: page 从 1 开始".to_string());
        }
        if args.quote.trim().is_empty() {
            return Err("cite_book: quote 不能为空".to_string());
        }
        let book = ReadBook::resolve_book(&args.book)?;
        if let Some(marker) = crate::pdf::read_parse_marker(Path::new(&book.path))
            && args.page > marker.page_count
        {
            return Err(format!(
                "cite_book: 页码超出范围（共 {} 页）",
                marker.page_count
            ));
        }
        Ok(ToolResult::ok(format!("已记录引用：第 {} 页", args.page)))
    }
}

impl Default for CiteBook {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl GenericsTool for CiteBook {
    async fn generics_execute(
        &self,
        ctx: &ToolContext,
        args: &CiteBookParams,
    ) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for CiteBook {
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
        let schema = CiteBook::new().schema();
        let obj = schema.as_object().expect("schema object");
        let props = obj["properties"].as_object().expect("properties");
        for field in ["book", "page", "quote"] {
            assert!(props.contains_key(field), "缺少字段 {field}");
        }
    }
}
