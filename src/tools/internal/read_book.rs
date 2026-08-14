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
            return Ok(ToolResult::ok(Self::search_pages_scored(
                &dir,
                &book.id,
                query,
                SEARCH_MAX_RESULTS,
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

    /// 评分式全书搜索：空格归一 + 去空格 OCR 兼容 + 多词评分 + 定义词加权 +
    /// 目录页降权，返回「[书ID] 第 N 页: 上下文」列表。
    pub(crate) fn search_pages_scored(
        dir: &Path,
        book_id: &str,
        query: &str,
        max_results: usize,
        max_chars: usize,
    ) -> String {
        let (terms, q_nospace) = tokenize_query(query);
        if terms.is_empty() && q_nospace.is_empty() {
            return "未找到匹配内容".to_string();
        }
        let pages_dir = crate::layout::book_pages_dir(dir);
        let mut entries: Vec<_> = std::fs::read_dir(&pages_dir)
            .ok()
            .into_iter()
            .flatten()
            .flatten()
            .collect();
        entries.sort_by_key(|e| e.file_name());

        struct Hit {
            page: u32,
            score: i32,
            line: usize,
            lines: Vec<String>,
        }

        let mut hits: Vec<Hit> = Vec::new();
        for entry in entries {
            let Ok(text) = std::fs::read_to_string(entry.path()) else {
                continue;
            };
            let page_no = entry
                .file_name()
                .to_string_lossy()
                .trim_end_matches(".md")
                .parse::<u32>()
                .unwrap_or(0);
            let lines: Vec<String> = text.lines().map(str::to_string).collect();
            let page_norm = collapse_whitespace(&text.to_lowercase());
            let page_nospace = remove_whitespace(&page_norm);
            let phrase_hit = !q_nospace.is_empty() && page_nospace.contains(&q_nospace);

            let mut matched = 0usize;
            let mut score = 0i32;
            for term in &terms {
                let count = page_nospace.matches(term.as_str()).count();
                if count > 0 {
                    matched += 1;
                    score += 10 * (count.min(5) as i32);
                }
            }
            if phrase_hit {
                score += 50;
            }
            if matched == 0 && !phrase_hit {
                continue;
            }
            if terms.len() > 1 && matched < 1 && !phrase_hit {
                continue;
            }
            if !terms.is_empty() && matched == terms.len() {
                score += 30; // 全词命中优先
            }
            score += definition_bonus(&page_nospace);
            score -= toc_penalty(&lines);
            if score <= 0 {
                continue;
            }
            let line = best_line_index(&lines, &terms, &q_nospace);
            hits.push(Hit {
                page: page_no,
                score,
                line,
                lines,
            });
        }

        hits.sort_by(|a, b| b.score.cmp(&a.score).then(a.page.cmp(&b.page)));
        let mut out: Vec<String> = Vec::new();
        let mut total = 0usize;
        for hit in hits {
            if out.len() >= max_results || total >= max_chars {
                break;
            }
            let lo = hit.line.saturating_sub(2);
            let hi = (hit.line + 2).min(hit.lines.len().saturating_sub(1));
            let ctx = hit.lines[lo..=hi]
                .iter()
                .map(|s| s.trim_end().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            let block = format!("[{book_id}] 第 {} 页:\n{ctx}", hit.page);
            total += block.chars().count() + 1;
            out.push(block);
        }
        if out.is_empty() {
            "未找到匹配内容".to_string()
        } else {
            out.join("\n\n")
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

// ── 评分式搜索辅助 ──────────────────────────────────────────────────────────

const SEARCH_STOPWORDS: &[&str] = &[
    "the", "of", "a", "an", "and", "or", "in", "on", "for", "to", "with", "is", "are",
    "was", "were", "be", "by", "at", "as", "that", "this", "it", "its", "from", "which",
    "的", "是", "一个", "一", "和", "与", "或", "在", "对", "于", "被", "把", "为",
];

const DEFINITION_KEYWORDS: &[&str] = &[
    "definition", "define", "defined", "axiom", "theorem", "lemma", "proposition",
    "定义", "公理", "定理", "命题", "group", "群",
];

fn collapse_whitespace(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn remove_whitespace(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn is_stopword(w: &str) -> bool {
    SEARCH_STOPWORDS.contains(&w)
}

fn tokenize_query(query: &str) -> (Vec<String>, String) {
    let norm = collapse_whitespace(&query.to_lowercase());
    let terms = norm
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && !is_stopword(s))
        .map(str::to_string)
        .collect::<Vec<_>>();
    let nospace = remove_whitespace(&norm);
    (terms, nospace)
}

fn definition_bonus(page_nospace: &str) -> i32 {
    if DEFINITION_KEYWORDS
        .iter()
        .any(|k| page_nospace.contains(k))
    {
        8
    } else {
        0
    }
}

fn toc_penalty(lines: &[String]) -> i32 {
    let mut toc = 0usize;
    let mut total = 0usize;
    for line in lines {
        let t = line.trim();
        if t.is_empty() {
            continue;
        }
        total += 1;
        let dots = t.contains("....") || t.contains("…") || t.contains(". . .");
        let first_digit = t.chars().next().map_or(false, |c| c.is_ascii_digit());
        let last_digit = t.chars().last().map_or(false, |c| c.is_ascii_digit());
        let chapter_like = t.to_lowercase().starts_with("chapter") && last_digit;
        if (dots && last_digit) || (first_digit && last_digit) || chapter_like {
            toc += 1;
        }
    }
    if total > 0 && toc * 100 / total >= 30 {
        30
    } else {
        0
    }
}

fn best_line_index(lines: &[String], terms: &[String], q_nospace: &str) -> usize {
    let mut best = 0usize;
    let mut best_score = -1i32;
    for (i, line) in lines.iter().enumerate() {
        let ln = remove_whitespace(&line.to_lowercase());
        let mut s = 0i32;
        if !q_nospace.is_empty() && ln.contains(q_nospace) {
            s += 20;
        }
        for term in terms {
            if ln.contains(term.as_str()) {
                s += 5;
            }
        }
        if s > best_score {
            best_score = s;
            best = i;
        }
    }
    best
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
    fn search_pages_scored_returns_context_and_ranks_definitions() {
        let dir =
            std::env::temp_dir().join(format!("ueberneon-readbook-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pages = crate::layout::book_pages_dir(&dir);
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(
            crate::layout::book_page_md_path(&pages, 1),
            "Chapter 7 Groups 169\n7.1 Definition and Examples of Groups 183\n7.2 Basic Properties of Groups 196",
        )
        .unwrap();
        std::fs::write(
            crate::layout::book_page_md_path(&pages, 2),
            "Definition 11.1 A group consists of a set G along with a binary operation\nthat satisfies associativity, identity and inverse axioms.",
        )
        .unwrap();
        let out = ReadBook::search_pages_scored(&dir, "book-1", "definition of a group", 10, 6000);
        let p2 = out.find("第 2 页").unwrap_or(usize::MAX);
        let p1 = out.find("第 1 页").unwrap_or(usize::MAX);
        assert!(p2 < p1, "定义页应排在目录页前面:\n{out}");
        assert!(out.contains("[book-1]"), "{out}");
        assert!(out.contains("Definition 11.1"), "{out}");
        let none = ReadBook::search_pages_scored(&dir, "book-1", "不存在词xyz", 10, 6000);
        assert_eq!(none, "未找到匹配内容");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_pages_scored_matches_ocr_glued_text() {
        let dir =
            std::env::temp_dir().join(format!("ueberneon-readbook-ocr-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pages = crate::layout::book_pages_dir(&dir);
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(
            crate::layout::book_page_md_path(&pages, 1),
            "RINGSMODULESGROUPSFIELDS introduction to abstract algebra",
        )
        .unwrap();
        let out = ReadBook::search_pages_scored(&dir, "b1", "groups", 5, 2000);
        assert!(out.contains("第 1 页"), "{out}");
        assert!(out.contains("GROUPS"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_pages_scored_falls_back_to_partial_terms() {
        let dir =
            std::env::temp_dir().join(format!("ueberneon-readbook-partial-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pages = crate::layout::book_pages_dir(&dir);
        std::fs::create_dir_all(&pages).unwrap();
        std::fs::write(
            crate::layout::book_page_md_path(&pages, 1),
            "The binary operation is closed on the set.",
        )
        .unwrap();
        std::fs::write(
            crate::layout::book_page_md_path(&pages, 2),
            "Identity and inverse elements are unique.",
        )
        .unwrap();
        let out = ReadBook::search_pages_scored(
            &dir,
            "b1",
            "binary operation associative identity inverse",
            10,
            4000,
        );
        assert!(out.contains("第 1 页") || out.contains("第 2 页"), "{out}");
        assert_ne!(out, "未找到匹配内容");
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
