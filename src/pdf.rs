// ── 书级 PDF 辅助 ──
//
// 知识库文本提取:把书的 original.pdf 逐页提取为 pages/NNNN.md,
// 完成后写入 parsed.json 标记;阅读器“本页文本”视图也从这里读取。
// 阅读器本身的页面渲染直接走 pdfium::PdfDocument,不经过 MD。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::layout;
use crate::pdfium;

/// parsed.json 内容
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseMarker {
    pub page_count: u32,
    pub completed_at: String,
}

/// 叠加层里的一个透明词(按词合并字符,拖动选区更稳定)。
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayWord {
    pub text: String,
    /// 距页面左边缘的百分比(0-100)
    pub left_pct: f64,
    /// 词顶距页面上边缘的距离(容器宽度单位 cqw,1cqw = 容器宽度 1%)
    pub top_cqw: f64,
    /// 词宽(容器宽度单位 cqw)
    pub width_cqw: f64,
    /// 词高(容器宽度单位 cqw)
    pub height_cqw: f64,
}

/// 叠加层的一行文本(块级,复制时提供换行)。
#[derive(Debug, Clone, PartialEq)]
pub struct OverlayLine {
    /// 距页面上边缘的百分比
    pub top_pct: f64,
    /// 行高(占页面高度的百分比)
    pub height_pct: f64,
    /// 行高(cqw 容器宽度单位)
    pub height_cqw: f64,
    /// 行内词,按 left 排序
    pub words: Vec<OverlayWord>,
}

/// 把 PDFium 字符盒(PDF 用户空间,原点左下)换算为透明文本层的相对坐标。
///
/// - `left% = left / w * 100`
/// - `top% = (h - top) / h * 100`
/// - `height_cqw = (top - bottom) / w * 100`
/// 普通字符按“垂直带重叠”贪心分行(兼容大小写/下伸部),退化字符
/// (空格等零宽/零高包围盒)就近挂到最近的行;行与词保持 PDFium
/// 内容顺序(即阅读顺序,多栏 PDF 不交错);每个字符仍有独立坐标,
/// 显示位置不受顺序影响;过滤控制字符(保留空格)。
pub fn build_text_overlay(
    chars: &[crate::pdfium::TextChar],
    page_width_pt: f64,
    page_height_pt: f64,
) -> Vec<OverlayLine> {
    if page_width_pt <= 0.0 || page_height_pt <= 0.0 {
        return Vec::new();
    }

    struct Item {
        index: usize,
        top_px: f64,
        bottom_px: f64,
        left_pt: f64,
        right_pt: f64,
        ch: char,
        degenerate: bool,
        height_cqw: f64,
    }
    struct LineAcc {
        top_px: f64,
        bottom_px: f64,
        chars: Vec<(usize, f64, f64, f64, f64, char)>,
    }

    // 过滤控制字符(保留空格);退化字符(零宽/零高)不丢弃,后续就近挂行
    let items: Vec<Item> = chars
        .iter()
        .enumerate()
        .filter(|(_, c)| !(c.ch.is_control() && c.ch != ' '))
        .map(|(index, c)| {
            let bottom_px = page_height_pt - c.bottom;
            let top_px = page_height_pt - c.top;
            let degenerate = c.right <= c.left || c.top <= c.bottom;
            Item {
                index,
                top_px,
                bottom_px,
                left_pt: c.left,
                right_pt: c.right,
                ch: c.ch,
                degenerate,
                height_cqw: if degenerate {
                    0.0
                } else {
                    (c.top - c.bottom) / page_width_pt * 100.0
                },
            }
        })
        .collect();
    if items.is_empty() {
        return Vec::new();
    }

    // 按 PDFium 内容顺序贪心分行(保持阅读顺序,不按视觉坐标重排):
    // 与当前行垂直带重叠 >= 25% 即并入,否则另起一行。
    let mut lines: Vec<LineAcc> = Vec::new();
    for item in items.iter().filter(|i| !i.degenerate) {
        let height = item.bottom_px - item.top_px;
        if let Some(line) = lines.last_mut() {
            let overlap = line.bottom_px.min(item.bottom_px) - line.top_px.max(item.top_px);
            if overlap > 0.0 && overlap >= 0.25 * height {
                line.top_px = line.top_px.min(item.top_px);
                line.bottom_px = line.bottom_px.max(item.bottom_px);
                line.chars.push((
                    item.index,
                    item.left_pt,
                    item.right_pt,
                    item.top_px,
                    item.height_cqw,
                    item.ch,
                ));
                continue;
            }
        }
        lines.push(LineAcc {
            top_px: item.top_px,
            bottom_px: item.bottom_px,
            chars: vec![(
                item.index,
                item.left_pt,
                item.right_pt,
                item.top_px,
                item.height_cqw,
                item.ch,
            )],
        });
    }

    // 退化字符(空格/零宽)挂到垂直中心所在或最近的行
    let flat: Vec<&Item> = items.iter().filter(|i| i.degenerate).collect();
    for item in flat {
        let center = (item.top_px + item.bottom_px) / 2.0;
        let mut best: Option<usize> = None;
        let mut best_dist = f64::INFINITY;
        for (idx, line) in lines.iter().enumerate() {
            let dist = if center >= line.top_px && center <= line.bottom_px {
                0.0
            } else if center < line.top_px {
                line.top_px - center
            } else {
                center - line.bottom_px
            };
            if dist < best_dist {
                best_dist = dist;
                best = Some(idx);
            }
        }
        if let Some(idx) = best {
            lines[idx].chars.push((
                item.index,
                item.left_pt,
                item.right_pt,
                item.top_px,
                item.height_cqw,
                item.ch,
            ));
        }
    }

    // 全是退化字符的页面:合并成单行
    if lines.is_empty() {
        let mut line = LineAcc {
            top_px: f64::INFINITY,
            bottom_px: f64::NEG_INFINITY,
            chars: Vec::new(),
        };
        for item in &items {
            line.top_px = line.top_px.min(item.top_px);
            line.bottom_px = line.bottom_px.max(item.bottom_px);
            line.chars.push((
                item.index,
                item.left_pt,
                item.right_pt,
                item.top_px,
                item.height_cqw,
                item.ch,
            ));
        }
        if !line.top_px.is_infinite() {
            lines.push(line);
        }
    }

    let mut out: Vec<OverlayLine> = Vec::with_capacity(lines.len());
    for a in lines {
        let height_pt = (a.bottom_px - a.top_px).max(0.001);
        let mut words = group_words(page_width_pt, a.chars);
        let height_cqw = words.iter().map(|w| w.height_cqw).fold(0.0, f64::max);
        // 退化词(纯空格等)补齐行高,保证拖选时命中区域连续
        for w in &mut words {
            if w.height_cqw <= 0.01 {
                w.height_cqw = height_cqw;
            }
        }
        out.push(OverlayLine {
            top_pct: a.top_px / page_height_pt * 100.0,
            height_pct: height_pt / page_height_pt * 100.0,
            height_cqw,
            words,
        });
    }
    out
}

/// 把一行内按内容顺序的字符合并成词:空格字符作为独立词(复制时自然
/// 保留间距),非空格字符与上一个词的水平间隙超过阈值时断词(兼容
/// 公式/排版碎片);保持内容顺序。
fn group_words(
    page_width_pt: f64,
    mut chars: Vec<(usize, f64, f64, f64, f64, char)>,
) -> Vec<OverlayWord> {
    const WORD_GAP_PT: f64 = 1.0;
    const MIN_WIDTH_PT: f64 = 0.3;

    chars.sort_by(|a, b| a.0.cmp(&b.0));

    let mut words: Vec<OverlayWord> = Vec::new();
    // 非空格词:text, left, right, 首字形 left, 末字形 right, top_px, height
    let mut cur: Option<(String, f64, f64, Option<f64>, Option<f64>, f64, f64)> = None;
    // 空格串词:text, left, right, top_px, height
    let mut spaces: Option<(String, f64, f64, f64, f64)> = None;
    for (_, left, right, top_px, height_cqw, ch) in chars {
        let right = right.max(left + MIN_WIDTH_PT);
        if ch == ' ' {
            // 结束当前非空格词,空格累计成独立词
            if let Some(c) = cur.take() {
                words.push(make_word(c, page_width_pt));
            }
            if let Some((text, _, s_right, s_top, s_height)) = spaces.as_mut() {
                text.push(' ');
                *s_right = (*s_right).max(right);
                *s_top = (*s_top).min(top_px);
                *s_height = (*s_height).max(height_cqw);
            } else {
                spaces = Some((" ".to_string(), left, right, top_px, height_cqw));
            }
        } else {
            // 结束空格串
            if let Some(s) = spaces.take() {
                words.push(make_space_word(s, page_width_pt));
            }
            if let Some((text, _, cur_right, glyph_left, glyph_right, cur_top, cur_height)) =
                cur.as_mut()
            {
                let gap = left - *cur_right;
                if gap > WORD_GAP_PT {
                    words.push(make_word(cur.take().unwrap(), page_width_pt));
                    cur = Some((
                        ch.to_string(),
                        left,
                        right,
                        Some(left),
                        Some(right),
                        top_px,
                        height_cqw,
                    ));
                } else {
                    text.push(ch);
                    *cur_right = (*cur_right).max(right);
                    if glyph_left.is_none() {
                        *glyph_left = Some(left);
                    }
                    *glyph_right = Some(glyph_right.map_or(right, |r| r.max(right)));
                    *cur_top = (*cur_top).min(top_px);
                    *cur_height = (*cur_height).max(height_cqw);
                }
            } else {
                cur = Some((
                    ch.to_string(),
                    left,
                    right,
                    Some(left),
                    Some(right),
                    top_px,
                    height_cqw,
                ));
            }
        }
    }
    if let Some(c) = cur.take() {
        words.push(make_word(c, page_width_pt));
    }
    if let Some(s) = spaces.take() {
        words.push(make_space_word(s, page_width_pt));
    }
    words
}

/// 空格串词:高亮范围极小,文本保留空格供复制。
fn make_space_word(
    (text, left, right, top_px, height_cqw): (String, f64, f64, f64, f64),
    page_width_pt: f64,
) -> OverlayWord {
    // 行尾/对齐产生的空格字形可能非常宽,封顶点击区域,避免盖住相邻列文字
    let width_cqw = (((right - left).max(0.3)) / page_width_pt * 100.0).min(2.0);
    OverlayWord {
        text,
        left_pct: left / page_width_pt * 100.0,
        top_cqw: top_px / page_width_pt * 100.0,
        width_cqw,
        height_cqw: height_cqw.max(0.01),
    }
}

fn make_word(
    (text, left, _right, glyph_left, glyph_right, top_px, height_cqw): (
        String,
        f64,
        f64,
        Option<f64>,
        Option<f64>,
        f64,
        f64,
    ),
    page_width_pt: f64,
) -> OverlayWord {
    // 高亮/命中范围只覆盖首尾非空格字形,空格保留在文本里供复制
    let left = glyph_left.unwrap_or(left);
    let right = glyph_right.unwrap_or(_right);
    OverlayWord {
        text,
        left_pct: left / page_width_pt * 100.0,
        top_cqw: top_px / page_width_pt * 100.0,
        width_cqw: ((right - left).max(0.3)) / page_width_pt * 100.0,
        height_cqw: height_cqw.max(0.01),
    }
}

/// 按 books 表 id 触发后台解析:打开 original.pdf、提取全部页面文本并写 marker。
pub fn parse_book(book_id: &str) -> Result<ParseMarker> {
    let book = crate::db::with_db(|conn| crate::books::get(conn, book_id))
        .map_err(|e| anyhow!("查询书籍失败:{e}"))?
        .ok_or_else(|| anyhow!("书籍不存在:{book_id}"))?;
    let dir = PathBuf::from(&book.path);
    let pdf_path = layout::book_pdf_path(&dir);
    extract_pdf_to_md(&pdf_path, &dir)
}

/// 把 PDF 逐页提取为 <book_dir>/pages/NNNN.md,完成后写 parsed.json。
pub fn extract_pdf_to_md(pdf_path: &Path, book_dir: &Path) -> Result<ParseMarker> {
    let doc = pdfium::open(pdf_path).map_err(|e| anyhow!("打开 PDF 失败:{e}"))?;
    let page_count = doc.page_count();
    let pages_dir = layout::book_pages_dir(book_dir);
    fs::create_dir_all(&pages_dir)
        .with_context(|| format!("创建 pages 目录失败:{}", pages_dir.display()))?;

    for page_index in 0..page_count {
        let text = doc
            .page_text(page_index)
            .map_err(|e| anyhow!("提取第 {} 页文本失败:{e}", page_index + 1))?;
        let md_path = layout::book_page_md_path(&pages_dir, page_index + 1);
        fs::write(&md_path, text)
            .with_context(|| format!("写入页面 MD 失败:{}", md_path.display()))?;
    }

    let marker = ParseMarker {
        page_count,
        completed_at: now_rfc3339(),
    };
    write_parse_marker(book_dir, &marker)?;
    Ok(marker)
}

/// 原子写 parsed.json(临时文件 + rename)。
pub fn write_parse_marker(book_dir: &Path, marker: &ParseMarker) -> Result<()> {
    let path = layout::book_parse_marker_path(book_dir);
    let json = serde_json::to_vec_pretty(marker).context("序列化 parsed.json 失败")?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, &json).with_context(|| format!("写入解析标记失败:{}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("发布解析标记失败:{}", path.display()))?;
    Ok(())
}

/// 读取解析标记;缺失或损坏返回 None。
pub fn read_parse_marker(book_dir: &Path) -> Option<ParseMarker> {
    let bytes = fs::read(layout::book_parse_marker_path(book_dir)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 读取某一页(1-based)的知识库文本;未解析或文件缺失返回 None。
pub fn page_text_file(book_dir: &Path, page_1based: u32) -> Option<String> {
    let pages_dir = layout::book_pages_dir(book_dir);
    fs::read_to_string(layout::book_page_md_path(&pages_dir, page_1based)).ok()
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_book_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ueberneon-pdf-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn marker_roundtrip() {
        let dir = temp_book_dir();
        let marker = ParseMarker {
            page_count: 42,
            completed_at: "2026-08-09T00:00:00+08:00".into(),
        };
        write_parse_marker(&dir, &marker).unwrap();
        assert_eq!(read_parse_marker(&dir), Some(marker));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_or_corrupt_marker_is_none() {
        let dir = temp_book_dir();
        assert_eq!(read_parse_marker(&dir), None);

        fs::create_dir_all(&dir).unwrap();
        fs::write(layout::book_parse_marker_path(&dir), "not json").unwrap();
        assert_eq!(read_parse_marker(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn page_md_path_and_text_helpers() {
        let dir = temp_book_dir();
        let pages_dir = layout::book_pages_dir(&dir);
        fs::create_dir_all(&pages_dir).unwrap();

        let md = layout::book_page_md_path(&pages_dir, 7);
        assert_eq!(md.file_name().unwrap().to_str().unwrap(), "0007.md");
        fs::write(&md, "线性代数").unwrap();
        assert_eq!(page_text_file(&dir, 7).as_deref(), Some("线性代数"));
        assert_eq!(page_text_file(&dir, 8), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_pdf_to_md_writes_pages_and_marker() {
        let dir = temp_book_dir();
        let pdf_path = dir.join("original.pdf");
        fs::write(&pdf_path, include_bytes!("../tests/fixtures/sample.pdf")).unwrap();

        let marker = extract_pdf_to_md(&pdf_path, &dir).unwrap();
        assert_eq!(marker.page_count, 1);
        assert_eq!(read_parse_marker(&dir), Some(marker.clone()));

        let md_path = layout::book_page_md_path(&layout::book_pages_dir(&dir), 1);
        let text = fs::read_to_string(&md_path).unwrap();
        assert!(text.contains("Hello PDFium"), "提取文本不完整:{text:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    // 已知限制:该 fixture 由 LibreOffice 生成,中文以“占位字形 + /ActualText”
    // 编码,PDFium 的文本页不会输出这些字符(Poppler 可以)。
    // 留作回归测试:实现 ActualText 解析后应取消 #[ignore] 并直接通过。
    #[test]
    #[ignore = "PDFium 不支持 /ActualText 占位字形,CJK 提取待后续实现"]
    fn extract_chinese_pdf_text() {
        let dir = temp_book_dir();
        let pdf_path = dir.join("original.pdf");
        fs::write(
            &pdf_path,
            include_bytes!("../tests/fixtures/sample-cjk.pdf"),
        )
        .unwrap();

        let marker = extract_pdf_to_md(&pdf_path, &dir).unwrap();
        assert_eq!(marker.page_count, 1);
        let text = fs::read_to_string(layout::book_page_md_path(&layout::book_pages_dir(&dir), 1))
            .unwrap();
        assert!(text.contains("你好"), "中文文本缺失:{text:?}");
        assert!(text.contains("PDFium"), "英文文本缺失:{text:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn overlay_converts_bottom_left_origin_to_top_left_percent() {
        let chars = vec![crate::pdfium::TextChar {
            ch: 'A',
            left: 10.0,
            bottom: 20.0,
            right: 20.0,
            top: 30.0,
        }];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        assert!(
            (line.top_pct - 85.0).abs() < 1e-9,
            "top_pct={}",
            line.top_pct
        );
        assert!(
            (line.height_pct - 5.0).abs() < 1e-9,
            "height_pct={}",
            line.height_pct
        );
        assert_eq!(line.words.len(), 1);
        let w = &line.words[0];
        assert_eq!(w.text, "A");
        assert!((w.left_pct - 10.0).abs() < 1e-9, "left_pct={}", w.left_pct);
        // 页面相对坐标:top=30pt,h=200pt -> top_px=170 -> 170cqw
        assert!((w.top_cqw - 170.0).abs() < 1e-9, "top_cqw={}", w.top_cqw);
        assert!(
            (w.width_cqw - 10.0).abs() < 1e-9,
            "width_cqw={}",
            w.width_cqw
        );
        assert!(
            (w.height_cqw - 10.0).abs() < 1e-9,
            "height_cqw={}",
            w.height_cqw
        );
    }

    #[test]
    fn overlay_caps_wide_space_hit_area() {
        let chars = vec![
            crate::pdfium::TextChar {
                ch: 'a',
                left: 0.0,
                bottom: 90.0,
                right: 10.0,
                top: 100.0,
            },
            crate::pdfium::TextChar {
                ch: ' ',
                left: 10.0,
                bottom: 90.0,
                right: 80.0,
                top: 100.0,
            },
            crate::pdfium::TextChar {
                ch: 'b',
                left: 80.0,
                bottom: 90.0,
                right: 90.0,
                top: 100.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 100.0);
        assert_eq!(lines.len(), 1);
        let words = &lines[0].words;
        assert_eq!(words.len(), 3);
        assert!(words[1].text.contains(' '), "空格文本保留");
        assert!(
            words[1].width_cqw <= 2.0 + 1e-9,
            "宽空格点击区域被封顶:{}",
            words[1].width_cqw
        );
    }

    #[test]
    fn overlay_groups_lines_preserves_content_order() {
        let chars = vec![
            crate::pdfium::TextChar {
                ch: 'a',
                left: 30.0,
                bottom: 90.0,
                right: 40.0,
                top: 100.0,
            },
            crate::pdfium::TextChar {
                ch: 'b',
                left: 10.0,
                bottom: 90.0,
                right: 20.0,
                top: 100.0,
            },
            crate::pdfium::TextChar {
                ch: 'c',
                left: 5.0,
                bottom: 40.0,
                right: 15.0,
                top: 50.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 2);
        // 保持内容顺序:a 在前,b 在后(即使 b 视觉上更靠左)
        assert_eq!(lines[0].words.len(), 1);
        assert_eq!(lines[0].words[0].text, "ab");
        assert_eq!(lines[1].words[0].text, "c");
    }

    #[test]
    fn overlay_filters_control_chars_and_keeps_space() {
        let chars = vec![
            crate::pdfium::TextChar {
                ch: '\n',
                left: 0.0,
                bottom: 0.0,
                right: 1.0,
                top: 1.0,
            },
            crate::pdfium::TextChar {
                ch: ' ',
                left: 10.0,
                bottom: 90.0,
                right: 11.0,
                top: 100.0,
            },
            crate::pdfium::TextChar {
                ch: 'A',
                left: 20.0,
                bottom: 90.0,
                right: 30.0,
                top: 100.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 1);
        let texts: Vec<&str> = lines[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec![" ", "A"]);
    }

    #[test]
    fn overlay_empty_input_returns_empty() {
        assert!(build_text_overlay(&[], 100.0, 200.0).is_empty());
        assert!(build_text_overlay(&[], 0.0, 200.0).is_empty());
    }

    #[test]
    fn overlay_groups_same_baseline_with_different_heights() {
        // 大写 H 更高,小写 e 更矮,但基线相同(bottom 相同),应同属一行
        let chars = vec![
            crate::pdfium::TextChar {
                ch: 'H',
                left: 0.0,
                bottom: 100.0,
                right: 10.0,
                top: 112.0,
            },
            crate::pdfium::TextChar {
                ch: 'e',
                left: 12.0,
                bottom: 100.0,
                right: 20.0,
                top: 106.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 1);
        let line = &lines[0];
        // 行顶取最高的 H:top_pct = (200-112)/200*100 = 44
        assert!(
            (line.top_pct - 44.0).abs() < 1e-9,
            "top_pct={}",
            line.top_pct
        );
        // H 与 e 间隙 >1pt 分成两个词;e 的行内偏移:
        // top_px_e=(200-106)=94, line_top_px=88 -> (94-88)/100*100=6
        assert_eq!(line.words.len(), 2);
        assert_eq!(line.words[0].text, "H");
        assert_eq!(line.words[1].text, "e");
        assert!(
            (line.words[1].top_cqw - 94.0).abs() < 1e-9,
            "top_cqw={}",
            line.words[1].top_cqw
        );
    }

    #[test]
    fn overlay_keeps_degenerate_space_in_line() {
        // 空格包围盒零宽(退化),仍应出现在行内并保持位置
        let chars = vec![
            crate::pdfium::TextChar {
                ch: 'A',
                left: 20.0,
                bottom: 90.0,
                right: 30.0,
                top: 100.0,
            },
            crate::pdfium::TextChar {
                ch: ' ',
                left: 40.0,
                bottom: 95.0,
                right: 40.0,
                top: 95.0,
            },
            crate::pdfium::TextChar {
                ch: 'B',
                left: 45.0,
                bottom: 90.0,
                right: 55.0,
                top: 100.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 1);
        // 空格作为独立词保留在 A 与 B 之间
        assert_eq!(lines[0].words.len(), 3);
        assert_eq!(lines[0].words[0].text, "A");
        assert_eq!(lines[0].words[1].text, " ");
        assert_eq!(lines[0].words[2].text, "B");
        assert!(
            (lines[0].words[0].width_cqw - 10.0).abs() < 1e-9,
            "width_cqw 异常:{}",
            lines[0].words[0].width_cqw,
        );
        assert!(
            lines[0].words[1].height_cqw > 0.0,
            "词高未补齐:{}",
            lines[0].words[1].height_cqw
        );
    }

    #[test]
    fn overlay_keeps_descender_in_same_line() {
        // g 有下伸部(bottom 更低),但与 e 同一基线,x-height 重叠足够,应同行
        let chars = vec![
            crate::pdfium::TextChar {
                ch: 'e',
                left: 0.0,
                bottom: 100.0,
                right: 8.0,
                top: 106.0,
            },
            crate::pdfium::TextChar {
                ch: 'g',
                left: 10.0,
                bottom: 92.0,
                right: 18.0,
                top: 106.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 1, "下伸部字母被错误拆行");
        let texts: Vec<&str> = lines[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["e", "g"]);
    }
}
