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
    /// 文字层词序起始 id（搜索结果或选区提供）；与 end_id 成对使用。
    #[schemars(description = "Start word id in the page text layer")]
    pub start_id: u32,
    /// 文字层词序结束 id（含）。
    #[schemars(description = "End word id in the page text layer (inclusive)")]
    pub end_id: u32,
    /// 可选展示文本；不传时用页面原文。
    #[schemars(description = "Optional display quote; page text is used when omitted")]
    pub quote: Option<String>,
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
        if args.start_id > args.end_id {
            return Err("cite_book: start_id 不能大于 end_id".to_string());
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
        let located = crate::quote_locator::locate_ids_in_book(
            &book.id,
            args.page,
            args.start_id,
            args.end_id,
        )?;
        let rects_json: Vec<serde_json::Value> = located
            .rects
            .iter()
            .map(|r| {
                serde_json::json!({
                    "left": r.left,
                    "top": r.top,
                    "width": r.width,
                    "height": r.height,
                })
            })
            .collect();
        let payload = serde_json::json!({
            "page": args.page,
            "quote": args.quote.clone().unwrap_or(located.text),
            "rects": rects_json,
        });
        Ok(ToolResult::ok(payload.to_string()))
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
        for field in ["book", "page", "start_id", "end_id"] {
            assert!(props.contains_key(field), "缺少字段 {field}");
        }
    }
}
