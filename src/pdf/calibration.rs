// ── 文档级排版参数校准 ──
//
// 从书的前几页自动测出阅读器用到的四个阈值:
//   列间距、段落缩进、公式带行距比例、小字比例。
// 结果缓存在 <书目录>/calibration.json,PDF 变化后自动重算。
// 所有阈值都有默认兜底,校准失败不影响阅读。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::OverlayLine;

const SAMPLE_PAGES: usize = 5;
const CALIBRATION_VERSION: u32 = 1;

/// 校准后的排版参数(与阅读器默认值同构)。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct DocCalibration {
    /// 列聚类:相邻行左缘间距超过该值视为新列(cqw)
    pub column_gap_cqw: f64,
    /// 段落判断:首行缩进超过该值视为新段落(cqw)
    pub paragraph_indent_cqw: f64,
    /// 公式句/公式块:垂直空隙超过该比例 × 行高即断块
    pub vertical_gap_ratio: f64,
    /// 小字判定:行高低于该比例 × 列中位行高进小字层
    pub small_height_ratio: f64,
}

impl Default for DocCalibration {
    fn default() -> Self {
        Self {
            column_gap_cqw: 2.0,
            paragraph_indent_cqw: 1.0,
            vertical_gap_ratio: 0.6,
            small_height_ratio: 0.91,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct CalibrationFile {
    version: u32,
    source_mtime: u64,
    calibration: DocCalibration,
}

fn calibration_path(book_dir: &Path) -> PathBuf {
    book_dir.join("calibration.json")
}

// ── 全局当前书校准 ──

static CURRENT: OnceLock<Mutex<Option<(String, DocCalibration)>>> = OnceLock::new();

/// 设置当前书的校准(阅读器打开书时调用)。
pub fn set(book_id: &str, cal: DocCalibration) {
    let mut guard = CURRENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    *guard = Some((book_id.to_string(), cal));
}

/// 当前书的校准;未设置时返回默认值。
pub fn current() -> DocCalibration {
    CURRENT
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .map(|(_, c)| *c)
        .unwrap_or_default()
}

/// 当前校准的列间距阈值。
pub fn column_gap_cqw() -> f64 {
    current().column_gap_cqw
}

/// 当前校准的段落缩进阈值。
pub fn paragraph_indent_cqw() -> f64 {
    current().paragraph_indent_cqw
}

/// 当前校准的公式带行距比例。
pub fn vertical_gap_ratio() -> f64 {
    current().vertical_gap_ratio
}

/// 当前校准的小字比例。
pub fn small_height_ratio() -> f64 {
    current().small_height_ratio
}

// ── 缓存读写 ──

fn source_mtime(book_dir: &Path) -> u64 {
    fs::metadata(book_dir.join("original.pdf"))
        .and_then(|m| m.modified())
        .map(|t| {
            t.duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0)
        })
        .unwrap_or(0)
}

/// 读取缓存;版本不匹配或 PDF 已变化时返回 None。
fn load(book_dir: &Path) -> Option<DocCalibration> {
    let path = calibration_path(book_dir);
    let bytes = fs::read(&path).ok()?;
    let file: CalibrationFile = serde_json::from_slice(&bytes).ok()?;
    if file.version != CALIBRATION_VERSION || file.source_mtime != source_mtime(book_dir) {
        return None;
    }
    Some(file.calibration)
}

fn save(book_dir: &Path, mtime: u64, cal: DocCalibration) -> Result<(), String> {
    let file = CalibrationFile {
        version: CALIBRATION_VERSION,
        source_mtime: mtime,
        calibration: cal,
    };
    let json = serde_json::to_string_pretty(&file).map_err(|e| e.to_string())?;
    fs::write(calibration_path(book_dir), json).map_err(|e| e.to_string())
}

/// 确保当前书已加载/计算校准,并设置为全局当前值。
pub fn ensure_for_book(
    book_id: &str,
    book_dir: &Path,
    doc: &crate::pdf::pdfium::PdfDocument,
) -> Result<DocCalibration, String> {
    let mtime = source_mtime(book_dir);
    if let Some(cal) = load(book_dir) {
        set(book_id, cal);
        return Ok(cal);
    }
    let cal = compute(doc)?;
    let _ = save(book_dir, mtime, cal);
    set(book_id, cal);
    Ok(cal)
}

// ── 校准计算 ──

/// 采样前几页并计算四项参数。
pub fn compute(doc: &crate::pdf::pdfium::PdfDocument) -> Result<DocCalibration, String> {
    let count = doc.page_count();
    let pages = (count as usize).min(SAMPLE_PAGES);
    if pages == 0 {
        return Ok(DocCalibration::default());
    }

    let mut page_lines: Vec<(Vec<LineStat>, f64, f64)> = Vec::new(); // (lines, page_w, page_h)
    for p in 0..pages {
        let chars = doc
            .page_text_chars(p as u32)
            .map_err(|e| format!("提取字符失败:{e}"))?;
        let (w, h) = doc
            .page_size(p as u32)
            .map_err(|e| format!("页面尺寸失败:{e}"))?;
        if w <= 0.0 || h <= 0.0 {
            continue;
        }
        let overlay = crate::pdf::build_text_overlay(&chars, w as f64, h as f64);
        let lines = collect_line_stats(&overlay, w as f64, h as f64);
        if lines.len() < 3 {
            continue; // 跳过封面/特殊页
        }
        page_lines.push((lines, w as f64, h as f64));
    }

    Ok(compute_from_lines(&page_lines))
}

/// 从采样页的行统计计算四项参数(纯函数,便于测试)。
fn compute_from_lines(pages: &[(Vec<LineStat>, f64, f64)]) -> DocCalibration {
    let mut left_gaps: Vec<f64> = Vec::new();
    for (lines, _, _) in pages {
        let mut sorted: Vec<&LineStat> = lines.iter().collect();
        sorted.sort_by(|a, b| a.left.partial_cmp(&b.left).unwrap());
        for pair in sorted.windows(2) {
            let gap = pair[1].left - pair[0].left;
            if gap > 0.05 {
                left_gaps.push(gap);
            }
        }
    }

    let column_gap_cqw = best_split(&mut left_gaps)
        .map(|b| b.clamp(1.0, 8.0))
        .unwrap_or(2.0);
    let mut cal = DocCalibration::default();
    cal.column_gap_cqw = column_gap_cqw;

    let mut vgaps: Vec<f64> = Vec::new();
    let mut indents: Vec<f64> = Vec::new();
    let mut height_ratios: Vec<f64> = Vec::new();
    let mut line_heights: Vec<f64> = Vec::new();

    for (lines, _, _) in pages {
        let columns = provisional_columns(lines, column_gap_cqw);
        for col_lines in &columns {
            if col_lines.len() < 2 {
                continue;
            }
            let col_left = col_lines
                .iter()
                .map(|l| l.left)
                .fold(f64::INFINITY, f64::min);
            let mut sorted = col_lines.clone();
            sorted.sort_by(|a, b| a.top_cqw.partial_cmp(&b.top_cqw).unwrap());
            for pair in sorted.windows(2) {
                let gap = pair[1].top_cqw - (pair[0].top_cqw + pair[0].height_cqw);
                vgaps.push(gap.max(0.0));
                if line_ends_sentence(&pair[0].last_text) {
                    let indent = pair[1].left - col_left;
                    if indent > 0.05 {
                        indents.push(indent);
                    }
                }
            }
            if col_lines.len() >= 4 {
                let mut hs: Vec<f64> = col_lines.iter().map(|l| l.height_cqw).collect();
                hs.sort_by(|a, b| a.partial_cmp(b).unwrap());
                let median_h = hs[hs.len() / 2].max(0.01);
                for l in col_lines {
                    height_ratios.push(l.height_cqw / median_h);
                    line_heights.push(l.height_cqw);
                }
            }
        }
    }

    cal.paragraph_indent_cqw = if indents.is_empty() {
        1.0
    } else {
        indents.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (indents[indents.len() / 2] * 0.6).clamp(0.4, 3.0)
    };

    cal.vertical_gap_ratio = match best_split(&mut vgaps) {
        Some(boundary) => {
            line_heights.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let median_h = line_heights
                .get(line_heights.len() / 2)
                .copied()
                .unwrap_or(1.0)
                .max(0.01);
            (boundary / median_h).clamp(0.3, 1.0)
        }
        None => 0.6,
    };

    cal.small_height_ratio = best_split(&mut height_ratios)
        .map(|b| b.clamp(0.7, 0.95))
        .unwrap_or(0.91);

    cal
}

/// 行统计(校准只用几何,不依赖阅读器分类)。
#[derive(Clone)]
struct LineStat {
    left: f64,
    top_cqw: f64,
    height_cqw: f64,
    last_text: String,
}

fn collect_line_stats(overlay: &[OverlayLine], page_w: f64, page_h: f64) -> Vec<LineStat> {
    let mut out = Vec::new();
    let ratio = if page_w > 0.0 { page_h / page_w } else { 1.0 };
    for line in overlay {
        let mut left = f64::INFINITY;
        let mut last_text = String::new();
        for w in &line.words {
            if w.text.trim().is_empty() {
                continue;
            }
            left = left.min(w.left_pct);
            last_text = w.text.clone();
        }
        if left.is_finite() {
            out.push(LineStat {
                left,
                top_cqw: line.top_pct * ratio,
                height_cqw: line.height_cqw,
                last_text,
            });
        }
    }
    out
}

fn line_ends_sentence(text: &str) -> bool {
    let t = text.trim_end();
    t.ends_with('.')
        || t.ends_with('!')
        || t.ends_with('?')
        || t.ends_with('。')
        || t.ends_with('！')
        || t.ends_with('？')
        || t.ends_with(';')
        || t.ends_with('；')
}

/// 用给定列间距做临时分列(按左缘排序,相邻间距 > gap 开新列)。
fn provisional_columns(lines: &[LineStat], gap: f64) -> Vec<Vec<LineStat>> {
    let mut sorted: Vec<LineStat> = lines.to_vec();
    sorted.sort_by(|a, b| a.left.partial_cmp(&b.left).unwrap());
    let mut cols: Vec<Vec<LineStat>> = Vec::new();
    let mut prev_left: Option<f64> = None;
    for line in sorted {
        if prev_left.is_none() || line.left - prev_left.unwrap() > gap {
            cols.push(Vec::new());
        }
        let left = line.left;
        cols.last_mut().unwrap().push(line);
        prev_left = Some(left);
    }
    cols
}

/// 一维两簇分割:返回簇间边界(两个簇均值的中点)。
/// 样本太少或簇不明显时返回 None(调用方用默认值)。
fn best_split(vals: &mut Vec<f64>) -> Option<f64> {
    vals.retain(|v| v.is_finite());
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = vals.len();
    if n < 8 {
        return None;
    }
    let sum: f64 = vals.iter().sum();
    let mean = sum / n as f64;
    let mut total_var = 0.0f64;
    for v in vals.iter() {
        total_var += (v - mean) * (v - mean);
    }
    total_var /= n as f64;
    if total_var <= 1e-9 {
        return None;
    }

    let mut best: Option<(usize, f64)> = None;
    let mut left_sum = 0.0;
    for k in 1..n {
        left_sum += vals[k - 1];
        let w0 = k as f64 / n as f64;
        let w1 = 1.0 - w0;
        if w0 < 0.2 || w1 < 0.2 {
            continue;
        }
        let m0 = left_sum / k as f64;
        let m1 = (sum - left_sum) / (n - k) as f64;
        // 两簇之间必须有明显空隙(大于任一簇自身跨度),否则视为同一簇
        let spread0 = vals[k - 1] - vals[0];
        let spread1 = vals[n - 1] - vals[k];
        if vals[k] - vals[k - 1] <= spread0.max(spread1) {
            continue;
        }
        let between = w0 * w1 * (m0 - m1) * (m0 - m1);
        if best.map_or(true, |(_, b)| between > b) {
            best = Some((k, between));
        }
    }
    let (k, between) = best?;
    // 簇间方差占比不足则视为单簇
    if between / total_var < 0.5 {
        return None;
    }
    Some((vals[k - 1] + vals[k]) / 2.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(left: f64, top_cqw: f64, height_cqw: f64, text: &str) -> LineStat {
        LineStat {
            left,
            top_cqw,
            height_cqw,
            last_text: text.to_string(),
        }
    }

    #[test]
    fn best_split_finds_two_clusters() {
        let mut vals = vec![0.2, 0.3, 0.25, 0.35, 5.0, 5.2, 4.8];
        // 7 个样本太少,补足
        vals.extend([0.28, 5.1]);
        let boundary = best_split(&mut vals).expect("应找到两簇");
        assert!(boundary > 1.0 && boundary < 4.0, "boundary={boundary}");
    }

    #[test]
    fn best_split_single_cluster_returns_none() {
        let mut vals = vec![0.2, 0.25, 0.3, 0.28, 0.22, 0.26, 0.24, 0.27, 0.29, 0.23];
        assert!(best_split(&mut vals).is_none());
    }

    #[test]
    fn compute_calibrates_column_gap() {
        // 两栏:栏内漂移 0.3,栏间距 5.0
        let mut lines = Vec::new();
        for i in 0..6 {
            lines.push(line(10.0 + i as f64 * 0.3, i as f64 * 10.0, 2.0, "a"));
            lines.push(line(15.3 + i as f64 * 0.3, i as f64 * 10.0, 2.0, "b"));
        }
        let cal = compute_from_lines(&[(lines, 100.0, 100.0)]);
        assert!(cal.column_gap_cqw > 1.5 && cal.column_gap_cqw < 4.0);
    }

    #[test]
    fn json_roundtrip_and_mtime() {
        let dir = std::env::temp_dir().join(format!("ueberneon-cal-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("original.pdf"), b"x").unwrap();
        let cal = DocCalibration {
            column_gap_cqw: 1.5,
            paragraph_indent_cqw: 0.8,
            vertical_gap_ratio: 0.7,
            small_height_ratio: 0.85,
        };
        let mtime = source_mtime(&dir);
        save(&dir, mtime, cal).unwrap();
        assert_eq!(load(&dir), Some(cal));
        // 损坏文件 → 视为失效
        fs::write(calibration_path(&dir), b"bad").unwrap();
        assert!(load(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }
}
