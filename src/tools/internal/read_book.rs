// read_book 工具 —— 读书:按书 ID/名称解析全局书库,支持按页读取
// (公式行自动 OCR 成 LaTeX)或按关键词搜索全书(pages/*.md)。
//
// 只读工具,不依赖项目工作区;路径严格限定在书目录内。

use std::path::{Path, PathBuf};

use crate::agent::{GenericsTool, ToolContext, ToolResult};
use crate::permission::Decision;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use schemars::JsonSchema;
use serde::Deserialize;
use ueberneon_macros::ToolMetaImpl;

/// 公式 OCR 渲染倍率(与阅读器「精确复制公式」一致)。
const FORMULA_SCALE: f32 = 4.0;
/// 搜索结果默认上限(条数)。
const SEARCH_MAX_RESULTS: usize = 40;

/// read_book —— 读取全局书库中的书。
///
/// 支持按页读取页面文本(公式行自动 OCR 成 LaTeX),或按关键词搜索
/// 全书并返回「页码: 片段」列表。
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(argType = ReadBookParams)]
pub struct ReadBook;

/// read_book 工具的输入参数。
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct ReadBookParams {
    /// 书名或书 ID(先按 ID 精确匹配,再按名称精确匹配)。
    #[schemars(description = "Book name or book id")]
    book: String,
    /// 1-based 页码;与 query 二选一。
    #[schemars(description = "1-based page number to read")]
    page: Option<u32>,
    /// 关键词搜索全书;与 page 二选一。
    #[schemars(description = "Search keyword across the book, returns page numbers and snippets")]
    query: Option<String>,
    /// 读取页面时是否把公式行 OCR 成 LaTeX(默认 true)。
    #[serde(default = "default_true")]
    #[schemars(description = "OCR formula lines to LaTeX (default true)")]
    include_formulas: bool,
    /// 返回内容上限(字符数,默认 6000)。
    #[serde(default = "default_max_chars")]
    #[schemars(
        range(min = 200, max = 20000),
        description = "Max characters to return (default 6000)"
    )]
    max_chars: usize,
}

fn default_true() -> bool {
    true
}

fn default_max_chars() -> usize {
    6000
}

impl ReadBook {
    pub fn new() -> Self {
        Self
    }

    /// 按 ID 或名称解析书(先 ID 后名称,均精确匹配)。
    pub(crate) fn resolve_book(book: &str) -> Result<crate::books::BookRow, String> {
        let book = book.trim();
        if book.is_empty() {
            return Err("read_book: book 不能为空".to_string());
        }
        let row = crate::db::with_db_result::<Option<crate::books::BookRow>, String>(|conn| {
            if let Some(row) = crate::books::get(conn, book).map_err(|e| e.to_string())? {
                return Ok(Some(row));
            }
            let rows = crate::books::list(conn).map_err(|e| e.to_string())?;
            Ok(match_book(&rows, book).cloned())
        })?
        .ok_or_else(|| {
            let candidates = crate::db::with_db_result::<Vec<String>, String>(|conn| {
                crate::books::list(conn)
                    .map(|rows| {
                        rows.iter()
                            .map(|r| r.name.clone())
                            .take(5)
                            .collect::<Vec<_>>()
                    })
                    .map_err(|e| e.to_string())
            })
            .unwrap_or_default();
            if candidates.is_empty() {
                format!("未找到书:{book}(书库为空)")
            } else {
                format!("未找到书:{book};现有书:{}", candidates.join(" | "))
            }
        })?;
        Ok(row)
    }

    async fn do_execute(
        &self,
        _ctx: &ToolContext,
        args: &ReadBookParams,
    ) -> Result<ToolResult, String> {
        if args.page.is_some() && args.query.is_some() {
            return Err("read_book: page 与 query 只能二选一".to_string());
        }
        let book = Self::resolve_book(&args.book)?;
        let dir = PathBuf::from(&book.path);

        if let Some(query) = &args.query {
            if query.trim().is_empty() {
                return Err("read_book: query 不能为空".to_string());
            }
            return Ok(ToolResult::ok(Self::search_pages(
                &dir,
                query,
                args.max_chars.max(200),
            )));
        }

        let Some(page) = args.page else {
            return Err("read_book: 需要提供 page 或 query".to_string());
        };
        if page == 0 {
            return Err("read_book: page 从 1 开始".to_string());
        }
        let include_formulas = args.include_formulas;
        let max_chars = args.max_chars.max(200);
        let out = tokio::task::spawn_blocking(move || {
            Self::read_page(&dir, page, include_formulas, max_chars)
        })
        .await
        .map_err(|e| format!("read_book: 任务失败:{e}"))??;
        Ok(ToolResult::ok(out))
    }

    /// 搜索 pages/*.md,返回「第 N 页: 片段」列表(大小写不敏感)。
    pub(crate) fn search_pages(dir: &Path, query: &str, max_chars: usize) -> String {
        let pages_dir = crate::layout::book_pages_dir(dir);
        let mut entries: Vec<_> = std::fs::read_dir(&pages_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());
        let q = query.to_lowercase();
        let mut out: Vec<String> = Vec::new();
        let mut total = 0usize;
        'pages: for entry in entries {
            if out.len() >= SEARCH_MAX_RESULTS || total >= max_chars {
                break;
            }
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let page_no = entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".md")
                .parse::<u32>()
                .unwrap_or(0);
            for line in text.lines() {
                if !line.to_lowercase().contains(&q) {
                    continue;
                }
                let snippet = format!("第 {page_no} 页: {}", line.trim());
                total += snippet.chars().count() + 1;
                if total > max_chars {
                    break 'pages;
                }
                out.push(snippet);
                if out.len() >= SEARCH_MAX_RESULTS {
                    break 'pages;
                }
            }
        }
        if out.is_empty() {
            "未找到匹配内容".to_string()
        } else {
            out.join("\n")
        }
    }

    /// 读取一页:优先带公式 OCR;任何失败回退 pages/NNNN.md 文本。
    fn read_page(
        dir: &Path,
        page: u32,
        include_formulas: bool,
        max_chars: usize,
    ) -> Result<String, String> {
        let fallback = || {
            let md = crate::layout::book_page_md_path(&crate::layout::book_pages_dir(dir), page);
            std::fs::read_to_string(&md)
                .map(|t| truncate(&t, max_chars))
                .map_err(|e| format!("read_book: 读取第 {page} 页失败:{e}"))
        };
        if !include_formulas {
            return fallback();
        }
        match Self::read_page_with_formula_ocr(dir, page, max_chars) {
            Ok(text) => Ok(text),
            Err(_) => fallback(),
        }
    }

    /// 渲染页面并逐公式行 OCR 成 LaTeX;失败返回 Err(调用方回退 md)。
    fn read_page_with_formula_ocr(
        dir: &Path,
        page: u32,
        max_chars: usize,
    ) -> Result<String, String> {
        let doc = crate::pdf::pdfium::open(&crate::layout::book_pdf_path(dir))
            .map_err(|e| format!("打开 PDF 失败:{e:#}"))?;
        let (w, h) = doc
            .page_size(page - 1)
            .map_err(|e| format!("页面尺寸失败:{e:#}"))?;
        let overlay = crate::pdf::overlay::page_overlay_lines(&doc, dir, page - 1, w, h)?
            .ok_or_else(|| "页面无词层(未配置页面 OCR)".to_string())?;
        let formula_lines = crate::pdf::overlay::formula_line_indices(&overlay);
        if formula_lines.is_empty() {
            let text = overlay
                .iter()
                .map(crate::pdf::overlay::line_text)
                .collect::<Vec<_>>()
                .join("\n");
            return Ok(truncate(&text, max_chars));
        }

        let png = doc
            .render_page_png(page - 1, FORMULA_SCALE)
            .map_err(|e| format!("渲染页面失败:{e:#}"))?;
        let img = image::load_from_memory(&png)
            .map_err(|e| format!("解码页面图像失败:{e}"))?
            .to_rgba8();

        let mut parts: Vec<String> = Vec::new();
        let mut formulas: Vec<String> = Vec::new();
        for (i, line) in overlay.iter().enumerate() {
            if !formula_lines.contains(&i) {
                parts.push(crate::pdf::overlay::line_text(line));
                continue;
            }
            let Some(bbox_cqw) = crate::pdf::overlay::line_bbox(line) else {
                parts.push(crate::pdf::overlay::line_text(line));
                continue;
            };
            let bbox = crate::pdf::overlay::bbox_px(bbox_cqw, w, FORMULA_SCALE);
            let latex = Self::formula_ocr_crop(&img, bbox)
                .unwrap_or_else(|_| crate::pdf::overlay::line_text(line));
            let n = formulas.len() + 1;
            formulas.push(format!("[公式{n}]: {latex}"));
            parts.push(format!("[公式{n}]"));
        }

        let mut out = parts.join("\n");
        if !formulas.is_empty() {
            out.push_str("\n\n公式:\n");
            out.push_str(&formulas.join("\n"));
        }
        Ok(truncate(&out, max_chars))
    }

    /// 裁剪公式区域(白边)并调用公式 OCR 后端,返回 LaTeX。
    fn formula_ocr_crop(
        img: &image::RgbaImage,
        bbox: (i32, i32, i32, i32),
    ) -> Result<String, String> {
        let (x, y, w, h) = bbox;
        let (x, y, w, h) = (
            x.clamp(0, img.width() as i32 - 1),
            y.clamp(0, img.height() as i32 - 1),
            w.clamp(1, img.width() as i32 - x),
            h.clamp(1, img.height() as i32 - y),
        );
        const PAD: u32 = 2;
        let mut canvas = image::RgbaImage::from_pixel(
            w as u32 + PAD * 2,
            h as u32 + PAD * 2,
            image::Rgba([255, 255, 255, 255]),
        );
        let crop =
            image::imageops::crop_imm(img, x as u32, y as u32, w as u32, h as u32).to_image();
        image::imageops::overlay(&mut canvas, &crop, PAD as i64, PAD as i64);
        let backend = crate::formula_ocr::backend_arc().map_err(|e| e.to_string())?;
        backend
            .recognize_rgba(canvas.as_raw(), canvas.width(), canvas.height())
            .map_err(|e| e.to_string())
    }
}

/// 书名匹配:精确(大小写不敏感)→ 前缀(唯一)→ 包含(唯一,长度 ≥ 8 防误配)。
fn match_book<'a>(
    rows: &'a [crate::books::BookRow],
    book: &str,
) -> Option<&'a crate::books::BookRow> {
    let lower = book.to_lowercase();
    if let Some(r) = rows
        .iter()
        .find(|r| r.name == book || r.name.eq_ignore_ascii_case(book))
    {
        return Some(r);
    }
    let prefixes: Vec<&crate::books::BookRow> = rows
        .iter()
        .filter(|r| r.name.to_lowercase().starts_with(&lower))
        .collect();
    if prefixes.len() == 1 {
        return Some(prefixes[0]);
    }
    if book.chars().count() >= 8 {
        let contains: Vec<&crate::books::BookRow> = rows
            .iter()
            .filter(|r| r.name.to_lowercase().contains(&lower))
            .collect();
        if contains.len() == 1 {
            return Some(contains[0]);
        }
    }
    None
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max_chars).collect();
    out.push_str("\n…(内容已截断)");
    out
}

#[async_trait::async_trait]
impl GenericsTool for ReadBook {
    async fn generics_execute(
        &self,
        ctx: &ToolContext,
        args: &ReadBookParams,
    ) -> Result<ToolResult, String> {
        self.do_execute(ctx, args).await
    }
}

#[async_trait::async_trait]
impl CheckableTool for ReadBook {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_keeps_char_boundary_and_appends_note() {
        let out = truncate("你好世界", 3);
        assert_eq!(out, "你好世\n…(内容已截断)");
        assert_eq!(truncate("short", 100), "short");
    }

    #[test]
    fn search_pages_returns_page_numbers_and_snippets() {
        let dir =
            std::env::temp_dir().join(format!("ueberneon-readbook-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pages = crate::layout::book_pages_dir(&dir);
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(
            crate::layout::book_page_md_path(&pages, 1),
            "线性代数定义\n正文内容",
        )
        .unwrap();
        std::fs::write(
            crate::layout::book_page_md_path(&pages, 2),
            "另一章\n代数几何简介",
        )
        .unwrap();
        let out = ReadBook::search_pages(&dir, "代数", 6000);
        assert!(out.contains("第 1 页"), "{out}");
        assert!(out.contains("第 2 页"), "{out}");
        assert!(out.contains("线性代数定义"), "{out}");
        let none = ReadBook::search_pages(&dir, "不存在词", 6000);
        assert_eq!(none, "未找到匹配内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_page_falls_back_to_md_when_pdf_missing() {
        let dir = std::env::temp_dir().join(format!(
            "ueberneon-readbook-fallback-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let pages = crate::layout::book_pages_dir(&dir);
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(crate::layout::book_page_md_path(&pages, 1), "fallback text").unwrap();
        let out = ReadBook::read_page(&dir, 1, true, 6000).unwrap();
        assert_eq!(out, "fallback text");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn match_book_falls_back_to_prefix_and_contains() {
        let rows = vec![
            crate::books::BookRow {
                id: "b1".into(),
                name: "Agentic Design Patterns A Hands-On Guide to Building Intelligent Systems (Antonio Gullí) (z-library.sk)"
                    .into(),
                path: "/tmp/b1".into(),
                created_at: "t".into(),
            },
            crate::books::BookRow {
                id: "b2".into(),
                name: "Other Book".into(),
                path: "/tmp/b2".into(),
                created_at: "t".into(),
            },
            crate::books::BookRow {
                id: "b3".into(),
                name: "Other Thing".into(),
                path: "/tmp/b3".into(),
                created_at: "t".into(),
            },
        ];
        assert_eq!(
            match_book(
                &rows,
                "agentic design patterns a hands-on guide to building intelligent systems (antonio gullí) (z-library.sk)"
            )
            .unwrap()
            .id,
            "b1"
        );
        assert_eq!(
            match_book(
                &rows,
                "Agentic Design Patterns A Hands-On Guide to Building Intelligent Systems (Antonio Gullí"
            )
            .unwrap()
            .id,
            "b1"
        );
        assert_eq!(
            match_book(&rows, "Intelligent Systems (Antonio")
                .unwrap()
                .id,
            "b1"
        );
        assert!(
            match_book(&rows, "Other").is_none(),
            "前缀/包含歧义时不应误配"
        );
    }
}
