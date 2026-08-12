// ── PDF 模块 ──
//
// 汇总所有 PDF 相关实现:
//   - pdfium:   自研 PDFium FFI 封装(渲染 + 字符盒)
//   - calibration: 文档级排版参数自动校准
//   - mod.rs:   书级辅助(知识库文本提取 pages/NNNN.md、overlay 构建)

pub mod calibration;
pub mod pdfium;
pub mod toc;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};

use crate::layout;

/// 字符盒宽度下限（pt）：退化包围盒也保留可命中的最小宽度。
const MIN_WIDTH_PT: f64 = 0.3;

/// parsed.json 内容
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ParseMarker {
    pub page_count: u32,
    pub completed_at: String,
}

/// 叠加层里的一个透明词(按词合并字符,拖动选区更稳定)。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayLine {
    /// 距页面上边缘的百分比
    pub top_pct: f64,
    /// 行高(占页面高度的百分比)
    pub height_pct: f64,
    /// 行高(cqw 容器宽度单位)
    pub height_cqw: f64,
    /// 行内非空格字符的字号中位数(点);全小写行字号与正文一致,
    /// 不会因为字形包围盒偏矮而被误判为小字。
    pub font_size_pt: f64,
    /// 行内词,按 left 排序
    pub words: Vec<OverlayWord>,
}

/// 页面是否有可用的 PDFium 文本(供知识库提取与扫描页判定)。
pub fn page_has_meaningful_text(chars: &[crate::pdf::pdfium::TextChar]) -> bool {
    chars
        .iter()
        .any(|c| !c.ch.is_control() && !c.ch.is_whitespace())
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
    chars: &[crate::pdf::pdfium::TextChar],
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
    let font_by_index: std::collections::HashMap<usize, f64> = chars
        .iter()
        .enumerate()
        .filter(|(_, c)| c.font_size > 0.0)
        .map(|(i, c)| (i, c.font_size))
        .collect();

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
        let mut fonts: Vec<f64> = a
            .chars
            .iter()
            .filter_map(|(idx, _, _, _, _, _)| font_by_index.get(idx).copied())
            .filter(|f| *f > 0.0)
            .collect();
        fonts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let font_size_pt = if fonts.is_empty() {
            0.0
        } else {
            fonts[(fonts.len() - 1) / 2]
        };
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
            font_size_pt,
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
    chars.sort_by(|a, b| a.0.cmp(&b.0));
    let gap_threshold = word_gap_threshold(&chars);

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
                if gap > gap_threshold {
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

/// 按行统计出一个自适应断词阈值：
/// - 字母间隙中位数 ×3（宽字距的书不把词拆散）；
/// - 下限为字号中位数 ×0.3（防止小间隙也误拆）；
/// - 至少 1pt，封顶 12pt（避免整行合成一个词）。
fn word_gap_threshold(chars: &[(usize, f64, f64, f64, f64, char)]) -> f64 {
    const BASE_GAP_PT: f64 = 1.0;
    const GAP_MULT: f64 = 3.0;
    const HEIGHT_RATIO: f64 = 0.3;
    const MAX_GAP_PT: f64 = 12.0;

    let mut gaps: Vec<f64> = Vec::new();
    let mut heights: Vec<f64> = Vec::new();
    let mut prev_right: Option<f64> = None;
    for (_, left, right, _, height_cqw, ch) in chars {
        if *ch == ' ' {
            prev_right = None;
            continue;
        }
        let right = right.max(left + MIN_WIDTH_PT);
        heights.push(*height_cqw);
        if let Some(pr) = prev_right {
            let gap = left - pr;
            if gap > 0.0 {
                gaps.push(gap);
            }
        }
        prev_right = Some(right);
    }

    let median = |mut v: Vec<f64>| -> Option<f64> {
        if v.is_empty() {
            return None;
        }
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Some(v[(v.len() - 1) / 2])
    };
    let from_gap = median(gaps).unwrap_or(0.0) * GAP_MULT;
    let from_height = median(heights).unwrap_or(0.0) * HEIGHT_RATIO;
    (from_gap.max(from_height).max(BASE_GAP_PT)).min(MAX_GAP_PT)
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
        // 无文本页(扫描页)跳过:由页面 OCR 子系统补写 pages/NNNN.md。
        if text.chars().any(|c| !c.is_control() && !c.is_whitespace()) {
            let md_path = layout::book_page_md_path(&pages_dir, page_index + 1);
            fs::write(&md_path, text)
                .with_context(|| format!("写入页面 MD 失败:{}", md_path.display()))?;
        }
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

// ── 页面 PNG 磁盘缓存 ──
//
// 渲染好的页面 PNG 按 书/页码/缩放 缓存到 <书目录>/cache/pages/，
// 重新打开或回看时直接读文件，跳过 PDFium 渲染 + PNG 编码；
// PDF 源文件（大小/mtime）变化时整体失效。

const MAX_CACHED_PAGE_PNGS: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PageCacheManifest {
    pdf_size: u64,
    pdf_mtime: u64,
}

static PAGE_CACHE_LOCK: Mutex<()> = Mutex::new(());

pub fn page_cache_dir(book_dir: &Path) -> PathBuf {
    book_dir.join("cache").join("pages")
}

fn page_cache_manifest_path(book_dir: &Path) -> PathBuf {
    page_cache_dir(book_dir).join("cache.json")
}

/// PDF 源文件标识：大小 + mtime（毫秒），用于缓存失效判断。
pub fn pdf_source_key(book_dir: &Path) -> Option<(u64, u64)> {
    let meta = fs::metadata(layout::book_pdf_path(book_dir)).ok()?;
    let mtime = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis() as u64;
    Some((meta.len(), mtime))
}

/// 打开书前调用：缓存与当前 PDF 不一致时清空并写新标记。
pub fn prepare_page_cache(book_dir: &Path) {
    let Some((pdf_size, pdf_mtime)) = pdf_source_key(book_dir) else {
        return;
    };
    let _guard = PAGE_CACHE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = page_cache_dir(book_dir);
    let valid = fs::read_to_string(page_cache_manifest_path(book_dir))
        .ok()
        .and_then(|s| serde_json::from_str::<PageCacheManifest>(&s).ok())
        .map(|m| m.pdf_size == pdf_size && m.pdf_mtime == pdf_mtime)
        .unwrap_or(false);
    if valid {
        return;
    }
    let _ = fs::remove_dir_all(&dir);
    let _ = fs::create_dir_all(&dir);
    let manifest = PageCacheManifest {
        pdf_size,
        pdf_mtime,
    };
    let tmp = dir.join("cache.json.tmp");
    let _ = fs::write(&tmp, serde_json::to_string(&manifest).unwrap_or_default());
    let _ = fs::rename(&tmp, page_cache_manifest_path(book_dir));
}

/// 读缓存 PNG（page 为 1-based）。
pub fn cached_page_png(book_dir: &Path, page_1based: u32, scale: f32) -> Option<Vec<u8>> {
    let path = page_cache_dir(book_dir).join(format!("p{page_1based:04}@s{scale:.1}.png"));
    fs::read(path).ok()
}

/// 写缓存 PNG 并做上限清理（保留页码最大、最近阅读的页）。
pub fn save_page_png(book_dir: &Path, page_1based: u32, scale: f32, png: &[u8]) {
    let _guard = PAGE_CACHE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let dir = page_cache_dir(book_dir);
    if !dir.is_dir() {
        let _ = fs::create_dir_all(&dir);
    }
    let path = dir.join(format!("p{page_1based:04}@s{scale:.1}.png"));
    let tmp = dir.join(format!("p{page_1based:04}.tmp-{}", std::process::id()));
    if fs::write(&tmp, png).is_ok() {
        let _ = fs::rename(&tmp, &path);
    }

    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut pages: Vec<u32> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.strip_prefix('p')
                .and_then(|s| s.split('@').next())
                .and_then(|s| s.parse::<u32>().ok())
        })
        .collect();
    if pages.len() > MAX_CACHED_PAGE_PNGS {
        pages.sort_unstable();
        let overflow = pages.len() - MAX_CACHED_PAGE_PNGS;
        for p in pages.into_iter().take(overflow) {
            let prefix = format!("p{p:04}@");
            if let Ok(entries) = fs::read_dir(&dir) {
                for e in entries.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.starts_with(&prefix) {
                        let _ = fs::remove_file(e.path());
                    }
                }
            }
        }
    }
}

fn now_rfc3339() -> String {
    chrono::Local::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_book_dir() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ueberneon-pdf-test-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
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
    fn page_png_cache_roundtrip_and_invalidates_on_pdf_change() {
        let dir = temp_book_dir();
        fs::write(layout::book_pdf_path(&dir), b"version-1").unwrap();
        prepare_page_cache(&dir);
        assert!(cached_page_png(&dir, 1, 3.0).is_none());

        save_page_png(&dir, 1, 3.0, b"png-bytes");
        assert_eq!(
            cached_page_png(&dir, 1, 3.0).as_deref(),
            Some(&b"png-bytes"[..])
        );
        // 不同缩放是独立缓存键
        assert!(cached_page_png(&dir, 1, 2.0).is_none());

        // PDF 源变化（大小不同）→ 缓存整体失效
        fs::write(layout::book_pdf_path(&dir), b"version-1-is-changed").unwrap();
        prepare_page_cache(&dir);
        assert!(cached_page_png(&dir, 1, 3.0).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_pdf_to_md_writes_pages_and_marker() {
        let dir = temp_book_dir();
        let pdf_path = dir.join("original.pdf");
        fs::write(&pdf_path, include_bytes!("../../tests/fixtures/sample.pdf")).unwrap();

        let marker = extract_pdf_to_md(&pdf_path, &dir).unwrap();
        assert_eq!(marker.page_count, 1);
        assert_eq!(read_parse_marker(&dir), Some(marker.clone()));

        let md_path = layout::book_page_md_path(&layout::book_pages_dir(&dir), 1);
        let text = fs::read_to_string(&md_path).unwrap();
        assert!(text.contains("Hello PDFium"), "提取文本不完整:{text:?}");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_pdf_to_md_skips_textless_scanned_pages() {
        let dir = temp_book_dir();
        let pdf_path = dir.join("original.pdf");
        fs::write(
            &pdf_path,
            include_bytes!("../../tests/fixtures/sample-scanned.pdf"),
        )
        .unwrap();

        let marker = extract_pdf_to_md(&pdf_path, &dir).unwrap();
        assert_eq!(marker.page_count, 1);
        assert_eq!(read_parse_marker(&dir), Some(marker.clone()));
        // 扫描页没有 PDFium 文本:不写空 MD,由页面 OCR 子系统补写。
        let md_path = layout::book_page_md_path(&layout::book_pages_dir(&dir), 1);
        assert!(!md_path.exists(), "无文本页不应生成空 pages/NNNN.md");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanned_fixture_renders_without_text_layer() {
        let dir = temp_book_dir();
        let pdf_path = dir.join("original.pdf");
        fs::write(
            &pdf_path,
            include_bytes!("../../tests/fixtures/sample-scanned.pdf"),
        )
        .unwrap();

        let doc = crate::pdf::pdfium::open(&pdf_path).unwrap();
        assert_eq!(doc.page_count(), 1);
        let chars = doc.page_text_chars(0).unwrap();
        assert!(
            crate::page_ocr::needs_ocr(&chars),
            "扫描 fixture 不应有 PDFium 文本"
        );
        let text = doc.page_text(0).unwrap();
        assert!(
            text.chars().all(|c| c.is_control() || c.is_whitespace()),
            "扫描 fixture 不应有可提取文本:{text:?}"
        );
        let png = doc.render_page_png(0, 1.0).unwrap();
        let img = image::load_from_memory(&png).unwrap();
        assert_eq!((img.width(), img.height()), (240, 100));
        let non_white = img
            .to_rgb8()
            .pixels()
            .filter(|p| p.0 != [255, 255, 255])
            .count();
        assert!(
            non_white > 100,
            "扫描 fixture 渲染结果几乎是空白:{non_white}"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn searchable_fixture_extracts_hidden_text_layer() {
        let dir = temp_book_dir();
        let pdf_path = dir.join("original.pdf");
        fs::write(
            &pdf_path,
            include_bytes!("../../tests/fixtures/sample-searchable.pdf"),
        )
        .unwrap();

        let doc = crate::pdf::pdfium::open(&pdf_path).unwrap();
        let text = doc.page_text(0).unwrap();
        assert!(text.contains("Hello OCR"), "提取文本不完整:{text:?}");
        assert!(text.contains("你好"), "中文字提取不完整:{text:?}");
        assert!(text.contains("scanned fixture"), "提取文本不完整:{text:?}");
        let chars = doc.page_text_chars(0).unwrap();
        assert!(
            !crate::page_ocr::needs_ocr(&chars),
            "可搜索 PDF 有文本层,不应触发 OCR"
        );
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
            include_bytes!("../../tests/fixtures/sample-cjk.pdf"),
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
        let chars = vec![crate::pdf::pdfium::TextChar {
            ch: 'A',
            left: 10.0,
            bottom: 20.0,
            right: 20.0,
            top: 30.0,
            font_size: 10.0,
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
            crate::pdf::pdfium::TextChar {
                ch: 'a',
                left: 0.0,
                bottom: 90.0,
                right: 10.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: ' ',
                left: 10.0,
                bottom: 90.0,
                right: 80.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'b',
                left: 80.0,
                bottom: 90.0,
                right: 90.0,
                top: 100.0,
                font_size: 10.0,
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
            crate::pdf::pdfium::TextChar {
                ch: 'a',
                left: 30.0,
                bottom: 90.0,
                right: 40.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'b',
                left: 10.0,
                bottom: 90.0,
                right: 20.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'c',
                left: 5.0,
                bottom: 40.0,
                right: 15.0,
                top: 50.0,
                font_size: 10.0,
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
            crate::pdf::pdfium::TextChar {
                ch: '\n',
                left: 0.0,
                bottom: 0.0,
                right: 1.0,
                top: 1.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: ' ',
                left: 10.0,
                bottom: 90.0,
                right: 11.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'A',
                left: 20.0,
                bottom: 90.0,
                right: 30.0,
                top: 100.0,
                font_size: 10.0,
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
    fn overlay_merges_same_baseline_with_different_heights() {
        // 大写 H 更高,小写 e 更矮,但基线相同(bottom 相同),应同属一行
        let chars = vec![
            crate::pdf::pdfium::TextChar {
                ch: 'H',
                left: 0.0,
                bottom: 100.0,
                right: 10.0,
                top: 112.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'e',
                left: 12.0,
                bottom: 100.0,
                right: 20.0,
                top: 106.0,
                font_size: 10.0,
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
        // H 与 e 的 2pt 间隙小于自适应阈值(字号中位数 9pt ×0.3),合并为一个词
        assert_eq!(line.words.len(), 1);
        assert_eq!(line.words[0].text, "He");
        assert!(
            (line.words[0].top_cqw - 88.0).abs() < 1e-9,
            "top_cqw={}",
            line.words[0].top_cqw
        );
    }

    #[test]
    fn overlay_merges_wide_letter_spacing_into_full_word() {
        // 宽字距的书:字母间隙 1.5pt,固定 1pt 阈值会把词拆散;自适应应合并
        let chars = vec![
            crate::pdf::pdfium::TextChar {
                ch: 'L',
                left: 0.0,
                bottom: 90.0,
                right: 6.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'a',
                left: 7.5,
                bottom: 90.0,
                right: 13.5,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 't',
                left: 15.0,
                bottom: 90.0,
                right: 19.0,
                top: 100.0,
                font_size: 10.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].words.len(), 1, "宽字距的词不应被拆散");
        assert_eq!(lines[0].words[0].text, "Lat");
    }

    #[test]
    fn overlay_splits_real_word_boundary_without_space_char() {
        // 没有空格字形的 PDF:词间隙明显大于字母间隙,仍应断词
        let chars = vec![
            crate::pdf::pdfium::TextChar {
                ch: 'a',
                left: 0.0,
                bottom: 90.0,
                right: 5.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'b',
                left: 6.0,
                bottom: 90.0,
                right: 11.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'c',
                left: 17.0,
                bottom: 90.0,
                right: 22.0,
                top: 100.0,
                font_size: 10.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        let texts: Vec<&str> = lines[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["ab", "c"], "词间隙应断词,字母间隙应合并");
    }

    #[test]
    fn overlay_keeps_degenerate_space_in_line() {
        // 空格包围盒零宽(退化),仍应出现在行内并保持位置
        let chars = vec![
            crate::pdf::pdfium::TextChar {
                ch: 'A',
                left: 20.0,
                bottom: 90.0,
                right: 30.0,
                top: 100.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: ' ',
                left: 40.0,
                bottom: 95.0,
                right: 40.0,
                top: 95.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'B',
                left: 45.0,
                bottom: 90.0,
                right: 55.0,
                top: 100.0,
                font_size: 10.0,
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
            crate::pdf::pdfium::TextChar {
                ch: 'e',
                left: 0.0,
                bottom: 100.0,
                right: 8.0,
                top: 106.0,
                font_size: 10.0,
            },
            crate::pdf::pdfium::TextChar {
                ch: 'g',
                left: 10.0,
                bottom: 92.0,
                right: 18.0,
                top: 106.0,
                font_size: 10.0,
            },
        ];
        let lines = build_text_overlay(&chars, 100.0, 200.0);
        assert_eq!(lines.len(), 1, "下伸部字母被错误拆行");
        let texts: Vec<&str> = lines[0].words.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(texts, vec!["eg"], "同词字母不应被拆开");
    }
}
