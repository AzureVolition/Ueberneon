// ── 引用定位（仅文字层词序 id）──
//
// CiteBook 只接收当页文字层的 start_id/end_id，工具直接按词序取原文与坐标。

/// 页面内引用矩形（left %, top/width/height cqw）。
#[derive(Clone, Debug, PartialEq)]
pub struct QuoteRect {
    pub left: f64,
    pub top: f64,
    pub width: f64,
    pub height: f64,
}

/// 定位结果：真实原文 + 坐标。
#[derive(Clone, Debug, PartialEq)]
pub struct LocatedQuote {
    pub text: String,
    pub rects: Vec<QuoteRect>,
}

struct Word {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    raw: String,
}

/// 按文字层词序 id 定位整段（start_id → end_id，含两端）。
pub fn locate_ids_in_overlay(
    lines: &[crate::pdf::OverlayLine],
    start_id: u32,
    end_id: u32,
) -> Option<LocatedQuote> {
    let mut words: Vec<Word> = Vec::new();
    for line in lines {
        for w in &line.words {
            words.push(Word {
                left: w.left_pct,
                top: w.top_cqw,
                width: w.width_cqw,
                height: w.height_cqw,
                raw: w.text.clone(),
            });
        }
    }
    if words.is_empty() {
        return None;
    }
    let start = (start_id as usize).min(words.len() - 1);
    let end = (end_id as usize).min(words.len() - 1);
    if start > end {
        return None;
    }
    let slice = &words[start..=end];
    let mut left = f64::INFINITY;
    let mut top = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for w in slice {
        left = left.min(w.left);
        top = top.min(w.top);
        right = right.max(w.left + w.width);
        bottom = bottom.max(w.top + w.height);
    }
    Some(LocatedQuote {
        text: slice
            .iter()
            .map(|w| w.raw.as_str())
            .collect::<Vec<_>>()
            .join(" "),
        rects: vec![QuoteRect {
            left,
            top,
            width: right - left,
            height: bottom - top,
        }],
    })
}

/// 打开书并按词序 id 定位。
pub fn locate_ids_in_book(
    book_id: &str,
    page: u32,
    start_id: u32,
    end_id: u32,
) -> Result<LocatedQuote, String> {
    if page == 0 {
        return Err("页码从 1 开始".to_string());
    }
    let book = crate::db::with_db(|conn| crate::books::get(conn, book_id))
        .map_err(|e| format!("查询书失败:{e}"))?
        .ok_or_else(|| format!("书不存在:{book_id}"))?;
    let dir = std::path::Path::new(&book.path);
    let doc = crate::pdf::pdfium::open(&crate::layout::book_pdf_path(dir))
        .map_err(|e| format!("打开 PDF 失败:{e:#}"))?;
    let (w, h) = doc
        .page_size(page - 1)
        .map_err(|e| format!("读取页面尺寸失败:{e}"))?;
    let lines = crate::pdf::overlay::page_overlay_lines(&doc, dir, page - 1, w, h)
        .map_err(|e| format!("读取页面文字层失败:{e}"))?
        .ok_or_else(|| "页面没有可用的文字层".to_string())?;
    locate_ids_in_overlay(&lines, start_id, end_id)
        .ok_or_else(|| "词序 id 超出该页文字层范围".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::{OverlayLine, OverlayWord};

    fn word(text: &str, left: f64, top: f64, width: f64) -> OverlayWord {
        OverlayWord {
            text: text.into(),
            left_pct: left,
            top_cqw: top,
            width_cqw: width,
            height_cqw: 2.0,
        }
    }

    fn sample_lines() -> Vec<OverlayLine> {
        vec![OverlayLine {
            top_pct: 10.0,
            height_pct: 2.0,
            height_cqw: 2.0,
            font_size_pt: 10.0,
            words: vec![
                word("Definition", 0.0, 10.0, 8.0),
                word("8.11", 9.0, 10.0, 4.0),
                word("An", 14.0, 10.0, 2.0),
                word("abelian", 17.0, 10.0, 6.0),
                word("group", 24.0, 10.0, 5.0),
                word("is", 30.0, 10.0, 2.0),
                word("finite", 33.0, 10.0, 5.0),
            ],
        }]
    }

    #[test]
    fn ids_locate_whole_span() {
        let located = locate_ids_in_overlay(&sample_lines(), 0, 6).unwrap();
        assert_eq!(located.rects.len(), 1, "整段应合并为一个包围盒");
        assert!(located.text.contains("Definition"), "{}", located.text);
        assert!(located.text.contains("finite"), "{}", located.text);
    }

    #[test]
    fn out_of_range_returns_none() {
        assert!(locate_ids_in_overlay(&sample_lines(), 0, 999).is_some());
        assert!(locate_ids_in_overlay(&sample_lines(), 5, 2).is_none());
    }
}
