// ── 共享页面词层与公式行提取 ──
//
// 阅读器的 ReadBook 工具与未来的知识库管线共用:获取页面词层
// (PDFium 文本层优先,扫描页回退页面 OCR),以及启发式公式行检测。
// 阅读器自身保留更精细的 classify_words 分类器,这里只提供工具所需子集。

use std::path::Path;

use crate::pdf::OverlayLine;
use crate::pdf::pdfium::PdfDocument;

/// 获取某页词层:文本页用 PDFium overlay,扫描页回退页面 OCR。
/// `Ok(None)` 表示页面无文本且 OCR 无结果(未配置/失败),调用方自行回退。
pub fn page_overlay_lines(
    doc: &PdfDocument,
    book_dir: &Path,
    page_index: u32,
    w: f32,
    h: f32,
) -> Result<Option<Vec<OverlayLine>>, String> {
    let chars = doc
        .page_text_chars(page_index)
        .map_err(|e| format!("{e:#}"))?;
    if crate::page_ocr::needs_ocr(&chars) {
        return match crate::page_ocr::overlay_for_page(book_dir, page_index + 1, doc) {
            Ok(Some(lines)) => Ok(Some(lines)),
            Ok(None) => Ok(None),
            Err(e) => Err(format!("页面 OCR 失败:{e}")),
        };
    }
    Ok(Some(crate::pdf::build_text_overlay(
        &chars, w as f64, h as f64,
    )))
}

/// 运算符字符(关系符 + 二元运算/数学符号),与阅读器分类器同一字符集。
fn is_operator_char(c: char) -> bool {
    matches!(
        c,
        '=' | '+'
            | '−'
            | '×'
            | '÷'
            | '±'
            | '<'
            | '>'
            | '≤'
            | '≥'
            | '≠'
            | '≡'
            | '≈'
            | '∝'
            | '∞'
            | '∂'
            | '∑'
            | '∏'
            | '∫'
            | '→'
            | '←'
            | '⇒'
            | '⇔'
            | '∀'
            | '∃'
            | '∈'
            | '∉'
            | '∪'
            | '∩'
            | '⊆'
            | '⊂'
            | '⊇'
            | '⊃'
            | '∧'
            | '∨'
            | '¬'
            | '∣'
            | '∥'
            | '·'
            | '⋅'
    )
}

/// 数学符号(Greek 字母、数学字母数字、星号)。
fn is_math_symbol_char(c: char) -> bool {
    let code = c as u32;
    (0x0370..=0x03FF).contains(&code)
        || (0x1D400..=0x1D7FF).contains(&code)
        || matches!(c, '∗' | '*')
}

/// 关系符(等式/不等式/集合关系)。
fn is_relation_char(c: char) -> bool {
    matches!(
        c,
        '=' | '≈' | '≠' | '≤' | '≥' | '<' | '>' | '≡' | '∈' | '∉' | '⊆' | '⊂' | '⊇' | '⊃'
    )
}

/// 行内非空格字符。
fn non_space_chars(line: &OverlayLine) -> usize {
    line.words
        .iter()
        .map(|w| w.text.chars().filter(|c| !c.is_whitespace()).count())
        .sum()
}

/// 启发式判定一行是否为公式行:出现关系符,或运算/数学符号密度足够高。
pub fn is_formula_line(line: &OverlayLine) -> bool {
    let total = non_space_chars(line);
    if total == 0 {
        return false;
    }
    let ops: usize = line
        .words
        .iter()
        .map(|w| w.text.chars().filter(|&c| is_operator_char(c)).count())
        .sum();
    let math: usize = line
        .words
        .iter()
        .map(|w| w.text.chars().filter(|&c| is_math_symbol_char(c)).count())
        .sum();
    let has_relation = line
        .words
        .iter()
        .any(|w| w.text.chars().any(is_relation_char));
    (has_relation && total >= 2) || (ops + math) as f64 / total as f64 >= 0.25
}

/// 返回被判定为公式行的行索引(按行顺序)。
pub fn formula_line_indices(overlay: &[OverlayLine]) -> Vec<usize> {
    overlay
        .iter()
        .enumerate()
        .filter(|(_, l)| is_formula_line(l))
        .map(|(i, _)| i)
        .collect()
}

/// 行内非空格词的包围盒,单位 cqw(均为页面宽度的百分比):
/// `(left, top, width, height)`。
pub fn line_bbox(line: &OverlayLine) -> Option<(f64, f64, f64, f64)> {
    let mut left = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut top = f64::INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for w in &line.words {
        if w.text.trim().is_empty() {
            continue;
        }
        left = left.min(w.left_pct);
        right = right.max(w.left_pct + w.width_cqw);
        top = top.min(w.top_cqw);
        bottom = bottom.max(w.top_cqw + w.height_cqw);
    }
    if !left.is_finite() {
        return None;
    }
    Some((left, top, right - left, bottom - top))
}

/// 把 cqw 包围盒换算成 @scale 像素坐标裁剪框(与阅读器同一换算规则)。
pub fn bbox_px(bbox: (f64, f64, f64, f64), page_width_pt: f32, scale: f32) -> (i32, i32, i32, i32) {
    let scale = page_width_pt as f64 * scale as f64;
    (
        ((bbox.0 / 100.0) * scale).floor().max(0.0) as i32,
        ((bbox.1 / 100.0) * scale).floor().max(0.0) as i32,
        ((bbox.2 / 100.0) * scale).ceil().max(1.0) as i32,
        ((bbox.3 / 100.0) * scale).ceil().max(1.0) as i32,
    )
}

/// 行文本:非空格词用空格连接(公式行由调用方替换为占位符)。
pub fn line_text(line: &OverlayLine) -> String {
    line.words
        .iter()
        .map(|w| w.text.as_str())
        .filter(|t| !t.trim().is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pdf::OverlayWord;

    fn word(text: &str, left: f64, top: f64, width: f64, height: f64) -> OverlayWord {
        OverlayWord {
            text: text.to_string(),
            left_pct: left,
            top_cqw: top,
            width_cqw: width,
            height_cqw: height,
        }
    }

    fn line(words: Vec<OverlayWord>) -> OverlayLine {
        OverlayLine {
            top_pct: words.first().map(|w| w.top_cqw).unwrap_or(0.0),
            height_pct: 1.0,
            height_cqw: 2.0,
            font_size_pt: 10.0,
            words,
        }
    }

    #[test]
    fn formula_line_detects_equations_and_skips_prose() {
        assert!(is_formula_line(&line(vec![
            word("y", 0.0, 0.0, 5.0, 2.0),
            word("=", 5.0, 0.0, 2.0, 2.0),
            word("ax", 7.0, 0.0, 6.0, 2.0),
            word("+", 13.0, 0.0, 2.0, 2.0),
            word("b", 15.0, 0.0, 3.0, 2.0),
        ])));
        assert!(is_formula_line(&line(vec![
            word("x", 0.0, 0.0, 3.0, 2.0),
            word("≤", 3.0, 0.0, 2.0, 2.0),
            word("1", 5.0, 0.0, 2.0, 2.0),
        ])));
        assert!(!is_formula_line(&line(vec![
            word("This", 0.0, 0.0, 10.0, 2.0),
            word("is", 10.0, 0.0, 4.0, 2.0),
            word("prose.", 14.0, 0.0, 12.0, 2.0),
        ])));
    }

    #[test]
    fn line_bbox_covers_non_space_words() {
        let l = line(vec![
            word("a", 10.0, 5.0, 8.0, 2.0),
            word(" ", 18.0, 5.0, 2.0, 2.0),
            word("b", 20.0, 6.0, 6.0, 3.0),
        ]);
        let b = line_bbox(&l).unwrap();
        assert!((b.0 - 10.0).abs() < 1e-6);
        assert!((b.1 - 5.0).abs() < 1e-6);
        assert!((b.2 - 16.0).abs() < 1e-6);
        assert!((b.3 - 4.0).abs() < 1e-6);
        let px = bbox_px(b, 600.0, 4.0);
        assert_eq!(px, (240, 120, 384, 96));
    }

    #[test]
    fn formula_line_indices_returns_line_numbers() {
        let overlay = vec![
            line(vec![word("hello", 0.0, 0.0, 10.0, 2.0)]),
            line(vec![
                word("E", 0.0, 0.0, 5.0, 2.0),
                word("=", 5.0, 0.0, 2.0, 2.0),
                word("mc²", 7.0, 0.0, 8.0, 2.0),
            ]),
        ];
        assert_eq!(formula_line_indices(&overlay), vec![1]);
    }
}
