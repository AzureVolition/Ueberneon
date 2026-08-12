//! 阅读器目录（TOC）。
//!
//! 数据源优先级：PDF 书签 → 正文字号识别 →（预留 OCR / 用户编辑）。
//! 结果持久化到 `<书目录>/toc.json`，PDF mtime 变化后自动重建；
//! 本阶段只读展示，字段与 UI 为 OCR/编辑预留。

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use super::pdfium::{Bookmark, TextChar};

const TOC_VERSION: u32 = 1;

/// 正文识别：行字号超过正文中位字号的该倍数才算标题。
const HEADING_FONT_RATIO: f64 = 1.18;
/// 一级标题字号倍数。
const LEVEL1_FONT_RATIO: f64 = 1.55;
/// 二级标题字号倍数。
const LEVEL2_FONT_RATIO: f64 = 1.25;
/// 同一视觉行聚类的容差（pt）。
const LINE_TOP_TOLERANCE_PT: f64 = 2.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TocSource {
    Bookmark,
    Auto,
    Ocr,
    Edited,
}

impl TocSource {
    pub fn label(&self) -> &'static str {
        match self {
            TocSource::Bookmark => "书签",
            TocSource::Auto => "识别",
            TocSource::Ocr => "OCR",
            TocSource::Edited => "已编辑",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TocItem {
    pub id: String,
    pub title: String,
    /// 1-based 页码。
    pub page: u32,
    /// 层级：0 = 一级。
    pub level: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<TocSource>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TocFile {
    pub version: u32,
    pub source_mtime: u64,
    pub source: TocSource,
    pub items: Vec<TocItem>,
}

pub fn toc_path(book_dir: &Path) -> PathBuf {
    book_dir.join("toc.json")
}

fn mtime_of(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

pub fn read_toc(book_dir: &Path) -> Option<TocFile> {
    let data = fs::read(toc_path(book_dir)).ok()?;
    let toc: TocFile = serde_json::from_slice(&data).ok()?;
    if toc.version != TOC_VERSION {
        return None;
    }
    Some(toc)
}

pub fn write_toc(book_dir: &Path, toc: &TocFile) -> std::io::Result<()> {
    fs::write(toc_path(book_dir), serde_json::to_vec_pretty(toc)?)
}

/// 读取目录；缺失、损坏、版本不符或 PDF 变化时重新生成并落盘。
pub fn load_or_generate(book_dir: &Path, force: bool) -> TocFile {
    let pdf_path = book_dir.join("original.pdf");
    let mtime = mtime_of(&pdf_path);
    if !force {
        if let Some(toc) = read_toc(book_dir) {
            if toc.source_mtime == mtime {
                return toc;
            }
        }
    }
    let toc = generate(&pdf_path, mtime);
    let _ = write_toc(book_dir, &toc);
    toc
}

fn generate(pdf_path: &Path, mtime: u64) -> TocFile {
    let bookmarks = crate::pdf::pdfium::open(pdf_path)
        .ok()
        .and_then(|doc| doc.bookmarks().ok())
        .unwrap_or_default();
    let items = from_bookmarks(&bookmarks);
    if !items.is_empty() {
        return TocFile {
            version: TOC_VERSION,
            source_mtime: mtime,
            source: TocSource::Bookmark,
            items,
        };
    }
    let items = auto_detect(pdf_path);
    TocFile {
        version: TOC_VERSION,
        source_mtime: mtime,
        source: TocSource::Auto,
        items,
    }
}

/// 书签 → 目录项（0-based 页转 1-based；无目标页的条目跳过）。
pub fn from_bookmarks(bookmarks: &[Bookmark]) -> Vec<TocItem> {
    bookmarks
        .iter()
        .filter_map(|b| {
            let page = b.page_index? + 1;
            let title = b.title.trim().to_string();
            if title.is_empty() {
                return None;
            }
            Some(TocItem {
                id: format!("b{page}-{}", b.level),
                title,
                page,
                level: b.level,
                source: None,
            })
        })
        .collect()
}

/// 正文识别：逐页用字号识别标题行（页眉/页脚通常同号或更小，不会命中）。
pub fn auto_detect(pdf_path: &Path) -> Vec<TocItem> {
    let Ok(doc) = crate::pdf::pdfium::open(pdf_path) else {
        return Vec::new();
    };
    let count = doc.page_count();
    let mut items: Vec<TocItem> = Vec::new();
    for page_idx in 0..count {
        let Ok(chars) = doc.page_text_chars(page_idx) else {
            continue;
        };
        for item in detect_headings_on_page(&chars, page_idx + 1) {
            if items.last().map(|l| l.title.as_str()) == Some(item.title.as_str()) {
                continue;
            }
            items.push(item);
        }
    }
    items
}

/// 单页标题识别（纯函数，便于单测）。
pub fn detect_headings_on_page(chars: &[TextChar], page: u32) -> Vec<TocItem> {
    let lines = group_chars_by_line(chars);
    if lines.is_empty() {
        return Vec::new();
    }
    let body_median = {
        let mut fonts: Vec<f64> = chars
            .iter()
            .filter(|c| !c.ch.is_whitespace() && c.font_size > 0.0)
            .map(|c| c.font_size)
            .collect();
        fonts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        fonts.get((fonts.len() - 1) / 2).copied().unwrap_or(0.0)
    };
    if body_median <= 0.0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    let mut seq = 0u32;
    for line in &lines {
        let text = line_text(line);
        if !is_heading_candidate(&text) {
            continue;
        }
        let font = line_median_font(line);
        let ratio = font / body_median;
        if ratio < HEADING_FONT_RATIO {
            continue;
        }
        let level = if ratio >= LEVEL1_FONT_RATIO {
            0
        } else if ratio >= LEVEL2_FONT_RATIO {
            1
        } else {
            2
        };
        seq += 1;
        out.push(TocItem {
            id: format!("a{page}-{seq}"),
            title: text,
            page,
            level,
            source: None,
        });
    }
    out
}

fn group_chars_by_line(chars: &[TextChar]) -> Vec<Vec<&TextChar>> {
    let mut lines: Vec<Vec<&TextChar>> = Vec::new();
    let mut tops: Vec<f64> = Vec::new();
    for c in chars {
        // 保留普通空格用于还原文本；换行/制表等控制空白不参与分行。
        if c.ch != ' ' && c.ch.is_whitespace() {
            continue;
        }
        let mut placed = false;
        for i in 0..lines.len() {
            if (c.top - tops[i]).abs() <= LINE_TOP_TOLERANCE_PT {
                lines[i].push(c);
                placed = true;
                break;
            }
        }
        if !placed {
            lines.push(vec![c]);
            tops.push(c.top);
        }
    }
    lines
}

fn line_median_font(line: &[&TextChar]) -> f64 {
    let mut fonts: Vec<f64> = line
        .iter()
        .filter(|c| c.font_size > 0.0)
        .map(|c| c.font_size)
        .collect();
    if fonts.is_empty() {
        return 0.0;
    }
    fonts.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    fonts[(fonts.len() - 1) / 2]
}

fn line_text(line: &[&TextChar]) -> String {
    let mut s = String::new();
    let mut prev_space = true;
    for c in line {
        if c.ch.is_whitespace() {
            prev_space = true;
            continue;
        }
        if !s.is_empty() && prev_space {
            s.push(' ');
        }
        s.push(c.ch);
        prev_space = false;
    }
    s.trim().to_string()
}

fn is_heading_candidate(text: &str) -> bool {
    let len = text.chars().count();
    if !(3..=120).contains(&len) {
        return false;
    }
    if text.chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    if text
        .chars()
        .filter(|c| !c.is_whitespace())
        .all(|c| c.is_ascii_punctuation())
    {
        return false;
    }
    true
}

/// 页码输入边界：解析 1..=total，非法/越界返回 None。
pub fn clamp_page(raw: &str, total: u32) -> Option<u32> {
    let n: i64 = raw.trim().parse().ok()?;
    if n < 1 || n > total as i64 {
        None
    } else {
        Some(n as u32)
    }
}

/// 按“收起集合”过滤目录项，返回可见项及“是否为父级（有子项）”标记。
/// 收起某父级时，其所有 level 更大的后代都被隐藏，直到同级或更高级条目。
pub fn visible_toc_items(items: &[TocItem], collapsed: &HashSet<String>) -> Vec<(TocItem, bool)> {
    let mut out = Vec::new();
    // 当前生效的“隐藏阈值”栈：值 = 被收起父级的 level + 1。
    let mut hidden: Vec<u32> = Vec::new();
    for (i, item) in items.iter().enumerate() {
        while let Some(&min_level) = hidden.last() {
            if item.level < min_level {
                hidden.pop();
            } else {
                break;
            }
        }
        if hidden
            .last()
            .is_some_and(|&min_level| item.level >= min_level)
        {
            continue;
        }
        let is_parent = items
            .get(i + 1)
            .map(|next| next.level > item.level)
            .unwrap_or(false);
        if is_parent && collapsed.contains(&item.id) {
            hidden.push(item.level + 1);
        }
        out.push((item.clone(), is_parent));
    }
    out
}

/// 进入阅读器时的默认收起集合：存在一级（level 0）条目时，
/// 把所有有子项的父级都收起，只显示章节骨架；没有一级条目时保持全展开。
pub fn default_collapsed(items: &[TocItem]) -> HashSet<String> {
    if !items.iter().any(|i| i.level == 0) {
        return HashSet::new();
    }
    items
        .windows(2)
        .filter(|w| w[1].level > w[0].level)
        .map(|w| w[0].id.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "ueberneon-toc-test-{}-{tag}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn char_at(ch: char, top: f64, font: f64) -> TextChar {
        TextChar {
            ch,
            left: 10.0,
            bottom: top - 8.0,
            right: 14.0,
            top,
            font_size: font,
        }
    }

    #[test]
    fn toc_roundtrip_and_mtime_invalidation() {
        let dir = tmp_dir("roundtrip");
        let toc = TocFile {
            version: TOC_VERSION,
            source_mtime: 123,
            source: TocSource::Auto,
            items: vec![TocItem {
                id: "a1-1".into(),
                title: "Introduction".into(),
                page: 1,
                level: 0,
                source: None,
            }],
        };
        write_toc(&dir, &toc).unwrap();
        assert_eq!(read_toc(&dir), Some(toc.clone()));
        // mtime 不符时 load_or_generate 会重建（PDF 不存在 → 空目录，但文件仍写）
        let regenerated = load_or_generate(&dir, false);
        assert_eq!(regenerated.source_mtime, 0);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_or_wrong_version_toc_is_none() {
        let dir = tmp_dir("corrupt");
        fs::write(toc_path(&dir), "not json").unwrap();
        assert_eq!(read_toc(&dir), None);
        fs::write(
            toc_path(&dir),
            r#"{"version":99,"source_mtime":1,"source":"auto","items":[]}"#,
        )
        .unwrap();
        assert_eq!(read_toc(&dir), None);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn bookmarks_take_precedence_over_auto() {
        let bms = vec![Bookmark {
            title: "Chapter 1".into(),
            page_index: Some(4),
            level: 0,
        }];
        let items = from_bookmarks(&bms);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].page, 5);
        assert_eq!(items[0].level, 0);
        // 无目标页的条目跳过
        let items = from_bookmarks(&[Bookmark {
            title: "No dest".into(),
            page_index: None,
            level: 1,
        }]);
        assert!(items.is_empty());
    }

    #[test]
    fn auto_detect_finds_larger_font_headings() {
        // 正文 10pt 两行，标题 16pt 一行
        let mut chars = Vec::new();
        let heading = "Introduction to Agents";
        for (i, ch) in heading.chars().enumerate() {
            chars.push(char_at(ch, 100.0, 16.0));
            chars[i].left = 10.0 + i as f64 * 9.0;
            chars[i].right = chars[i].left + 8.0;
        }
        // 空格字符：有坐标、同字号，参与分行但不应产生额外文本。
        for (i, ch) in heading.chars().enumerate() {
            if ch == ' ' {
                chars.push(char_at(' ', 100.0, 16.0));
                let last = chars.len() - 1;
                chars[last].left = 10.0 + i as f64 * 9.0;
                chars[last].right = chars[last].left + 3.0;
            }
        }
        for (i, ch) in "This is the body text line one.".chars().enumerate() {
            let mut c = char_at(ch, 80.0, 10.0);
            c.left = 10.0 + i as f64 * 6.0;
            c.right = c.left + 5.0;
            chars.push(c);
        }
        for (i, ch) in "And another normal line here.".chars().enumerate() {
            let mut c = char_at(ch, 60.0, 10.0);
            c.left = 10.0 + i as f64 * 6.0;
            c.right = c.left + 5.0;
            chars.push(c);
        }
        let items = detect_headings_on_page(&chars, 3);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Introduction to Agents");
        assert_eq!(items[0].page, 3);
        assert_eq!(items[0].level, 0);
    }

    #[test]
    fn auto_detect_skips_page_numbers_and_small_lines() {
        let mut chars = Vec::new();
        for (i, ch) in "42".chars().enumerate() {
            chars.push(char_at(ch, 120.0, 16.0));
            chars[i].left = 300.0 + i as f64 * 9.0;
            chars[i].right = chars[i].left + 8.0;
        }
        for (i, ch) in "normal body text here.".chars().enumerate() {
            let mut c = char_at(ch, 90.0, 10.0);
            c.left = 10.0 + i as f64 * 6.0;
            c.right = c.left + 5.0;
            chars.push(c);
        }
        let items = detect_headings_on_page(&chars, 2);
        assert!(items.is_empty(), "{items:?}");
    }

    #[test]
    fn clamp_page_bounds() {
        assert_eq!(clamp_page("1", 10), Some(1));
        assert_eq!(clamp_page("10", 10), Some(10));
        assert_eq!(clamp_page("0", 10), None);
        assert_eq!(clamp_page("11", 10), None);
        assert_eq!(clamp_page("abc", 10), None);
        assert_eq!(clamp_page("", 10), None);
    }

    fn item(id: &str, level: u32, title: &str) -> TocItem {
        TocItem {
            id: id.into(),
            title: title.into(),
            page: 1,
            level,
            source: None,
        }
    }

    #[test]
    fn visible_toc_items_marks_parents_and_collapses_subtree() {
        let items = vec![
            item("a", 0, "Chapter 1"),
            item("a1", 1, "Section 1.1"),
            item("a1x", 2, "Subsection"),
            item("a2", 1, "Section 1.2"),
            item("b", 0, "Chapter 2"),
        ];
        let empty = HashSet::new();
        let all = visible_toc_items(&items, &empty);
        assert_eq!(all.len(), 5);
        assert!(all[0].1 && all[1].1, "a/a1 是父级");
        assert!(!all[2].1 && !all[3].1 && !all[4].1, "叶子不是父级");

        let mut collapsed = HashSet::new();
        collapsed.insert("a".to_string());
        let visible = visible_toc_items(&items, &collapsed);
        let ids: Vec<&str> = visible.iter().map(|(t, _)| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "收起 Chapter 1 后隐藏整棵子树");

        collapsed.insert("a1".to_string());
        collapsed.remove("a");
        let visible = visible_toc_items(&items, &collapsed);
        let ids: Vec<&str> = visible.iter().map(|(t, _)| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["a", "a1", "a2", "b"],
            "收起嵌套子级时同级 a2 不被吞掉"
        );

        collapsed.remove("a1");
        let visible = visible_toc_items(&items, &collapsed);
        assert_eq!(visible.len(), 5, "恢复展开后全部可见");
    }

    #[test]
    fn default_collapsed_hides_parents_when_root_exists() {
        let items = vec![
            item("a", 0, "Chapter 1"),
            item("a1", 1, "Section"),
            item("a1x", 2, "Subsection"),
            item("b", 0, "Chapter 2"),
        ];
        let collapsed = default_collapsed(&items);
        assert!(collapsed.contains("a") && collapsed.contains("a1"));
        assert!(!collapsed.contains("b"));
        let visible = visible_toc_items(&items, &collapsed);
        let ids: Vec<&str> = visible.iter().map(|(t, _)| t.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b"], "默认只显示一级章");
    }

    #[test]
    fn default_collapsed_stays_expanded_without_root_level() {
        let items = vec![item("x", 1, "No Root"), item("y", 2, "Child")];
        assert!(default_collapsed(&items).is_empty());
    }
}
