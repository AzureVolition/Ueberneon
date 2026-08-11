// ── 全屏阅读器(连续滚动 + 双文字层) ──
//
// 连续滚动浏览 PDF;每页文字拆成「正文层」与「小字层」(脚注/角标/
// 竖排侧边小字),两层交互逻辑一致(拖动/双击/三击/Cmd+双击/Cmd+C),
// 但拖动禁止跨层。句子/段落在本层内按列过滤,未结束时可向后跨 1 页。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use base64::Engine as _;
use dioxus::desktop::use_window;
use dioxus::prelude::*;

use crate::formula_ocr::SingleSlotCache;
use crate::pdf::{OverlayLine, parse_book};
use crate::pdfium::{self, PdfDocument};
use crate::ui::components::error::{ErrorInfo, ErrorSeverity, ErrorSignal, ErrorSource};

/// 固定渲染质量:2.0 像素/点 = 144dpi。
const RENDER_SCALE: f32 = 2.0;
/// OCR 裁剪渲染倍率:4.0 像素/点 = 288dpi,提升公式识别精度。
const OCR_RENDER_SCALE: f32 = 4.0;
/// 内存中最多保留的已渲染页数
const MAX_CACHE_PAGES: usize = 60;
/// 每次滚动到底加载的页数
const BATCH_SIZE: u32 = 5;
/// 距底部多远时触发加载下一批(像素)
const NEAR_BOTTOM_PX: f64 = 900.0;
const ZOOM_MIN: u32 = 50;
const ZOOM_MAX: u32 = 400;
const ZOOM_STEP: u32 = 25;
/// 列聚类阈值(cqw)
/// 小字判定:行高低于同列中位数的该比例(实测脚注约为正文的 0.90)
/// 窄列判定:列内行宽中位数小于正文中位宽的该比例 → 候选公式列
const NARROW_COLUMN_WIDTH_RATIO: f64 = 0.5;
/// 单行窄文本判定:行宽小于该值(cqw)视为角标/碎片
const NARROW_LINE_WIDTH_CQW: f64 = 2.0;
/// 公式行:运算符字符占非空格字符的比例阈值
const OPERATOR_DENSITY_THRESHOLD: f64 = 0.12;
/// 公式行/列:数学符号(Greek/Unicode)密度阈值
const MATH_SYMBOL_DENSITY_THRESHOLD: f64 = 0.04;
/// 竖排列:单字形行占比阈值
const VERTICAL_TEXT_FRAGMENT_RATIO: f64 = 0.5;
/// 竖排列:单字形行高宽比阈值
const VERTICAL_TEXT_ASPECT: f64 = 2.0;
/// 竖排列:与其它列的最小左缘距离(cqw)
const VERTICAL_TEXT_ISOLATED_GAP_CQW: f64 = 4.0;
/// 关系符 + 短行判定公式行的宽度比例
const FORMULA_RELATION_WIDTH_RATIO: f64 = 0.6;
/// 公式置信度阈值:选区置信度 ≥ 该值才允许弹「精确复制公式」操作栏。
/// 后续可移到设置页,当前先作为可调常量。
const FORMULA_SCORE_THRESHOLD: f64 = 0.45;
/// 归一化满密度:运算符密度达到该值即算满分
const OPERATOR_DENSITY_FULL: f64 = 0.30;
/// 归一化满密度:数学符号密度达到该值即算满分
const MATH_DENSITY_FULL: f64 = 0.10;
/// 操作栏成功文案停留时间
const ACTION_BAR_SUCCESS_MS: u64 = 2500;
/// 翻译卡片相对操作栏的垂直偏移(px)。
const ACTION_BAR_CARD_OFFSET_Y: f64 = 38.0;

/// 文字层
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Layer {
    Body,
    Small,
}

/// 内容顺序展平后的一个词。
#[derive(Clone)]
struct FlatWord {
    line: usize,
    text: String,
    left_cqw: f64,
    top_cqw: f64,
    width_cqw: f64,
    height_cqw: f64,
    line_height_cqw: f64,
    /// 该词所在行是否被判定为公式(用于选区/复制语义)。
    formula: bool,
    /// 公式置信度 0..1:密度 + 关系符 + 列/隔离/缩进上下文。
    formula_score: f64,
    /// 交互用索引:空格词吸附到同一行最近的真实词
    gesture: usize,
}

/// 自绘选区矩形。
#[derive(Clone, Copy)]
struct SelectionRect {
    left_cqw: f64,
    top_cqw: f64,
    width_cqw: f64,
    height_cqw: f64,
}

/// 阅读路径上的一步:某页某列内的连续区间。
#[derive(Debug, Clone, Copy)]
struct SelectionStep {
    page: u32,
    lo: usize,
    hi: usize,
    /// Some(列代表左缘):该步只包含这一列;None:手动拖动,不过滤。
    column_left: Option<f64>,
}

/// 选区:按阅读顺序排列的步骤(可跨栏、最多跨 1 页)。
#[derive(Debug, Clone)]
struct Selection {
    layer: Layer,
    steps: Vec<SelectionStep>,
    /// 选区是否按公式处理(OCR 复制 / 公式断句)。
    formula: bool,
    /// 锚点词的公式置信度(0..1)。
    formula_score: f64,
}

/// 操作栏状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionBarStatus {
    Idle,
    Loading,
    Error,
    Success,
}

/// 浮动操作栏:位置 + 状态 + 代数(用于异步复位)。
#[derive(Debug, Clone, Copy)]
struct ActionBarState {
    x: f64,
    y: f64,
    status: ActionBarStatus,
    generation: u64,
    /// 是否显示「精确复制公式」(仅公式选区显示)。
    show_formula: bool,
    /// 是否显示「翻译」(已配置翻译模型且选区含正文)。
    translation_enabled: bool,
}

/// 翻译卡片状态机:加载中 / 成功译文 / 失败文案。
#[derive(Debug, Clone)]
struct TranslationCardState {
    x: f64,
    y: f64,
    status: ActionBarStatus,
    generation: u64,
    text: String,
}

/// 一次公式复制请求(由单 worker 串行处理)。
#[derive(Clone)]
struct CopyRequest {
    /// 0-based 页码(渲染层用)。
    page: usize,
    key: String,
    /// OCR 裁剪框(像素,@4x):(x, y, w, h)
    bbox: (i32, i32, i32, i32),
    doc: Arc<PdfDocument>,
    /// OCR 失败时的文本层重建结果
    fallback: String,
}

/// 一页的渲染结果:data URI + 两层词表 + 页面宽高。
struct RenderedPage {
    src: String,
    body: Vec<FlatWord>,
    small: Vec<FlatWord>,
    w_pt: f32,
    h_pt: f32,
}

struct ReaderSession {
    doc: Arc<PdfDocument>,
    page_count: u32,
    cache: HashMap<u32, RenderedPage>,
    rendered_until: u32,
    loading_more: bool,
    book_id: String,
    book_name: String,
    selection: Option<Selection>,
    drag_anchor: Option<(u32, Layer, usize)>,
    dragging: bool,
    /// 单槽缓存:最近一次 (key, LaTeX);相同 key 直接复用。
    ocr_cache: SingleSlotCache,
    /// 待处理的最新复制请求(在途时新请求会覆盖它)。
    pending_copy: Option<CopyRequest>,
    /// 是否已有复制 worker 在运行(同一时刻最多一个 OCR)。
    copy_busy: bool,
    /// 公式选区右键操作栏(仅公式选区显示)。
    action_bar: Option<ActionBarState>,
    /// 操作栏代数:每次打开 +1,异步状态复位据此丢弃过期更新。
    action_bar_gen: u64,
    /// 翻译卡片(操作栏下方)。
    translation: Option<TranslationCardState>,
    /// 翻译卡片代数:每次发起翻译 +1,过期响应据此丢弃。
    translation_gen: u64,
}

/// 渲染页面并拆分两层词表(阻塞,供 spawn_blocking 调用)。
fn render_page_with_overlay(doc: &PdfDocument, page_index: u32) -> Result<RenderedPage, String> {
    let png = doc
        .render_page_png(page_index, RENDER_SCALE)
        .map_err(|e| format!("{e:#}"))?;
    let src = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    );
    let chars = doc
        .page_text_chars(page_index)
        .map_err(|e| format!("{e:#}"))?;
    let (w, h) = doc.page_size(page_index).map_err(|e| format!("{e:#}"))?;
    let overlay = crate::pdf::build_text_overlay(&chars, w as f64, h as f64);
    let (body, small) = classify_words(&overlay);
    Ok(RenderedPage {
        src,
        body,
        small,
        w_pt: w,
        h_pt: h,
    })
}

fn build_flat(overlay: &[OverlayLine]) -> Vec<FlatWord> {
    let mut flat = Vec::new();
    for (line_idx, line) in overlay.iter().enumerate() {
        for w in &line.words {
            flat.push(FlatWord {
                line: line_idx,
                text: w.text.clone(),
                left_cqw: w.left_pct,
                top_cqw: w.top_cqw,
                width_cqw: w.width_cqw,
                height_cqw: w.height_cqw,
                line_height_cqw: line.height_cqw,
                formula: false,
                formula_score: 0.0,
                gesture: 0,
            });
        }
    }
    assign_gestures(&mut flat);
    flat
}

/// 空格词吸附到同一行最近的真实词。
fn assign_gestures(flat: &mut [FlatWord]) {
    for i in 0..flat.len() {
        if !flat[i].text.trim().is_empty() {
            flat[i].gesture = i;
            continue;
        }
        let line = flat[i].line;
        let mut g = i;
        while g > 0 {
            let prev = &flat[g - 1];
            if prev.line != line || prev.text.trim().is_empty() {
                g -= 1;
            } else {
                break;
            }
        }
        if g > 0 {
            flat[i].gesture = g - 1;
            continue;
        }
        let mut g = i;
        while g + 1 < flat.len() {
            let next = &flat[g + 1];
            if next.line != line || next.text.trim().is_empty() {
                g += 1;
            } else {
                break;
            }
        }
        flat[i].gesture = if g + 1 < flat.len() { g + 1 } else { i };
    }
}

/// 运算符字符(关系符 + 二元运算/数学符号)。
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

fn median_sorted(vals: &[f64]) -> f64 {
    if vals.is_empty() {
        0.0
    } else {
        vals[vals.len() / 2]
    }
}

/// 把整页词表拆成正文层/小字层,并给正文层词标上公式标记:
/// - 竖排列(隔离窄列 + 高瘦字形 + 均匀行距)整列进小字层;
/// - 窄列(行宽 < 0.5× 正文中位宽)且含公式信号且 ≥3 行 → 公式列,
///   整列留在正文层(包括 `p`/`∗` 等单字符碎片);
/// - 非窄列中的公式行(运算符密度或“关系符+短行”)标记为公式行;
/// - 同列小字(行高 < 0.91× 列中位数)与单行窄碎片仍进小字层。
fn classify_words(overlay: &[OverlayLine]) -> (Vec<FlatWord>, Vec<FlatWord>) {
    classify_words_with(overlay, crate::calibration::current())
}

fn classify_words_with(
    overlay: &[OverlayLine],
    cal: crate::calibration::DocCalibration,
) -> (Vec<FlatWord>, Vec<FlatWord>) {
    let flat = build_flat(overlay);

    // 行几何与内容统计(仅非空格词)
    let mut line_left: HashMap<usize, f64> = HashMap::new();
    let mut line_right: HashMap<usize, f64> = HashMap::new();
    let mut line_top: HashMap<usize, f64> = HashMap::new();
    let mut line_height: HashMap<usize, f64> = HashMap::new();
    let mut line_words: HashMap<usize, usize> = HashMap::new();
    let mut line_chars: HashMap<usize, usize> = HashMap::new();
    let mut line_ops: HashMap<usize, usize> = HashMap::new();
    let mut line_math: HashMap<usize, usize> = HashMap::new();
    let mut line_relation: HashMap<usize, bool> = HashMap::new();
    for w in &flat {
        if w.text.trim().is_empty() {
            continue;
        }
        let l = line_left.entry(w.line).or_insert(f64::INFINITY);
        *l = (*l).min(w.left_cqw);
        let r = line_right.entry(w.line).or_insert(f64::NEG_INFINITY);
        *r = (*r).max(w.left_cqw + w.width_cqw);
        let t = line_top.entry(w.line).or_insert(f64::INFINITY);
        *t = (*t).min(w.top_cqw);
        let h = line_height.entry(w.line).or_insert(0.0);
        *h = (*h).max(w.line_height_cqw);
        *line_words.entry(w.line).or_insert(0) += 1;
        let chars = w.text.chars().filter(|c| !c.is_whitespace()).count();
        *line_chars.entry(w.line).or_insert(0) += chars;
        *line_ops.entry(w.line).or_insert(0) +=
            w.text.chars().filter(|&c| is_operator_char(c)).count();
        *line_math.entry(w.line).or_insert(0) +=
            w.text.chars().filter(|&c| is_math_symbol_char(c)).count();
        if w.text.chars().any(is_relation_char) {
            line_relation.insert(w.line, true);
        }
    }

    // 单字形行:该行只有一个词且只有一个非空格字符(用于竖排检测)
    let mut line_single: HashMap<usize, bool> = HashMap::new();
    for (line, count) in &line_words {
        if *count == 1 {
            if let Some(w) = flat
                .iter()
                .find(|w| w.line == *line && !w.text.trim().is_empty())
            {
                if w.text.chars().filter(|c| !c.is_whitespace()).count() == 1 {
                    line_single.insert(*line, true);
                }
            }
        }
    }

    // 列聚类(校准后的列间距阈值)
    let mut line_ids: Vec<usize> = line_left.keys().copied().collect();
    line_ids.sort_by(|a, b| {
        line_left[a]
            .partial_cmp(&line_left[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut column_of: HashMap<usize, usize> = HashMap::new();
    let mut column_lines: Vec<Vec<usize>> = Vec::new();
    let mut prev_left: Option<f64> = None;
    for lid in &line_ids {
        if let Some(prev) = prev_left {
            if line_left[lid] - prev > cal.column_gap_cqw {
                column_lines.push(Vec::new());
            }
        } else {
            column_lines.push(Vec::new());
        }
        let col = column_lines.len() - 1;
        column_of.insert(*lid, col);
        column_lines[col].push(*lid);
        prev_left = Some(line_left[lid]);
    }

    // 每列统计:中位高度/宽度、内容密度、碎片比例、字形高宽比、垂直间隙
    let col_count = column_lines.len();
    let mut col_median_height = vec![0.0; col_count];
    let mut col_median_width = vec![0.0; col_count];
    let mut col_left = vec![f64::INFINITY; col_count];
    let mut col_chars = vec![0usize; col_count];
    let mut col_ops = vec![0usize; col_count];
    let mut col_math = vec![0usize; col_count];
    let mut col_relation = vec![false; col_count];
    let mut col_fragment_ratio = vec![0.0; col_count];
    let mut col_glyph_aspects: Vec<Vec<f64>> = vec![Vec::new(); col_count];
    let mut col_gaps: Vec<Vec<f64>> = vec![Vec::new(); col_count];
    for (col, lines) in column_lines.iter().enumerate() {
        let mut hs: Vec<f64> = lines
            .iter()
            .map(|l| line_height.get(l).copied().unwrap_or(0.0))
            .collect();
        hs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        col_median_height[col] = median_sorted(&hs);

        let mut ws: Vec<f64> = lines
            .iter()
            .map(|l| (line_right[l] - line_left[l]).max(0.0))
            .collect();
        ws.sort_by(|a, b| a.partial_cmp(b).unwrap());
        col_median_width[col] = median_sorted(&ws);
        col_left[col] = lines
            .iter()
            .map(|l| line_left[l])
            .fold(f64::INFINITY, f64::min);
        for l in lines {
            col_chars[col] += line_chars.get(l).copied().unwrap_or(0);
            col_ops[col] += line_ops.get(l).copied().unwrap_or(0);
            col_math[col] += line_math.get(l).copied().unwrap_or(0);
            if line_relation.get(l).copied().unwrap_or(false) {
                col_relation[col] = true;
            }
            if line_single.get(l).copied().unwrap_or(false) {
                let width = (line_right[l] - line_left[l]).max(0.001);
                col_glyph_aspects[col].push(line_height.get(l).copied().unwrap_or(0.0) / width);
            }
        }
        let fragments = lines
            .iter()
            .filter(|l| line_single.get(l).copied().unwrap_or(false))
            .count();
        col_fragment_ratio[col] = if lines.is_empty() {
            0.0
        } else {
            fragments as f64 / lines.len() as f64
        };
        let mut sorted_lines = lines.clone();
        sorted_lines.sort_by(|a, b| {
            line_top[a]
                .partial_cmp(&line_top[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for pair in sorted_lines.windows(2) {
            let gap = line_top[&pair[1]] - (line_top[&pair[0]] + line_height[&pair[0]]);
            col_gaps[col].push(gap.max(0.0));
        }
        col_gaps[col].sort_by(|a, b| a.partial_cmp(b).unwrap());
        col_glyph_aspects[col].sort_by(|a, b| a.partial_cmp(b).unwrap());
    }

    // 正文中位行宽:优先取 ≥4 行的列(再取其中较宽的一半,避免公式列
    // 拉低中位数);退化时取全页行宽中位数
    let mut cand_widths: Vec<f64> = column_lines
        .iter()
        .enumerate()
        .filter(|(_, lines)| lines.len() >= 4)
        .map(|(i, _)| col_median_width[i])
        .collect();
    cand_widths.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let body_median_width = if cand_widths.is_empty() {
        let mut all: Vec<f64> = line_ids
            .iter()
            .map(|l| (line_right[l] - line_left[l]).max(0.0))
            .collect();
        all.sort_by(|a, b| a.partial_cmp(b).unwrap());
        median_sorted(&all)
    } else {
        let initial = median_sorted(&cand_widths);
        let upper: Vec<f64> = cand_widths
            .iter()
            .copied()
            .filter(|w| *w >= initial)
            .collect();
        if upper.is_empty() {
            initial
        } else {
            median_sorted(&upper)
        }
    };

    let vertical_text = |col: usize| -> bool {
        let lines = &column_lines[col];
        if lines.len() < 3 {
            return false;
        }
        if col_fragment_ratio[col] < VERTICAL_TEXT_FRAGMENT_RATIO {
            return false;
        }
        if median_sorted(&col_glyph_aspects[col]) < VERTICAL_TEXT_ASPECT {
            return false;
        }
        for (other, _) in column_lines.iter().enumerate() {
            if other == col {
                continue;
            }
            if (col_left[other] - col_left[col]).abs() <= VERTICAL_TEXT_ISOLATED_GAP_CQW {
                return false;
            }
        }
        let mg = median_sorted(&col_gaps[col]);
        let mh = col_median_height[col].max(0.001);
        mg >= 0.7 * mh && mg <= 1.6 * mh
    };

    let formula_col = |col: usize| -> bool {
        if vertical_text(col) {
            return false;
        }
        if column_lines[col].len() < 3 {
            return false;
        }
        let narrow = col_median_width[col] < body_median_width * NARROW_COLUMN_WIDTH_RATIO;
        if !narrow {
            return false;
        }
        let chars = col_chars[col] as f64;
        chars > 0.0
            && (col_ops[col] as f64 / chars >= OPERATOR_DENSITY_THRESHOLD
                || col_math[col] as f64 / chars >= MATH_SYMBOL_DENSITY_THRESHOLD
                || col_relation[col])
    };

    let line_formula = |line: usize| -> bool {
        let chars = line_chars.get(&line).copied().unwrap_or(0) as f64;
        if chars <= 0.0 {
            return false;
        }
        let ops = line_ops.get(&line).copied().unwrap_or(0) as f64;
        let width = (line_right.get(&line).copied().unwrap_or(0.0)
            - line_left.get(&line).copied().unwrap_or(0.0))
        .max(0.0);
        ops / chars >= OPERATOR_DENSITY_THRESHOLD
            || (line_relation.get(&line).copied().unwrap_or(false)
                && width < body_median_width * FORMULA_RELATION_WIDTH_RATIO)
    };

    // 行上下文:是否被空行隔离(前后行距都超过垂直间隙阈值)。
    let mut line_iso: HashMap<usize, bool> = HashMap::new();
    for lines in column_lines.iter() {
        let mut sorted = lines.clone();
        sorted.sort_by(|a, b| {
            line_top[a]
                .partial_cmp(&line_top[b])
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (i, &l) in sorted.iter().enumerate() {
            let h = line_height.get(&l).copied().unwrap_or(0.0).max(0.001);
            let gap_before = if i == 0 {
                0.0
            } else {
                line_top[&l] - (line_top[&sorted[i - 1]] + line_height[&sorted[i - 1]])
            };
            let gap_after = if i + 1 == sorted.len() {
                0.0
            } else {
                line_top[&sorted[i + 1]] - (line_top[&l] + h)
            };
            let threshold = h * cal.vertical_gap_ratio.max(0.001);
            line_iso.insert(
                l,
                gap_before.max(0.0) > threshold && gap_after.max(0.0) > threshold,
            );
        }
    }

    // 公式置信度 0..1:密度 + 关系符 + 窄列/隔离/缩进上下文。
    let formula_score_for = |line: usize, col: usize| -> f64 {
        let chars = line_chars.get(&line).copied().unwrap_or(0) as f64;
        if chars <= 0.0 {
            return 0.0;
        }
        let ops = line_ops.get(&line).copied().unwrap_or(0) as f64;
        let math = line_math.get(&line).copied().unwrap_or(0) as f64;
        let rel = line_relation.get(&line).copied().unwrap_or(false);
        let ops_d = (ops / chars / OPERATOR_DENSITY_FULL).min(1.0);
        let math_d = (math / chars / MATH_DENSITY_FULL).min(1.0);
        let narrow_col = formula_col(col);
        let isolated = line_iso.get(&line).copied().unwrap_or(false);
        let centered = line_left.get(&line).copied().unwrap_or(0.0) - col_left[col] >= 2.0;
        let mut s = 0.0;
        s += ops_d * 0.40;
        s += math_d * 0.25;
        if rel {
            s += 0.15;
        }
        if narrow_col {
            s += 0.20;
            // 公式列本身是强信号:列内碎片(`p`/`∗`)也继承高置信度。
            s = s.max(0.65);
        }
        if isolated {
            s += 0.10;
        }
        if centered {
            s += 0.05;
        }
        s.min(1.0)
    };

    let is_small_line = |line: usize| -> bool {
        let Some(&col) = column_of.get(&line) else {
            return false;
        };
        if vertical_text(col) {
            return true;
        }
        if formula_col(col) {
            return false;
        }
        if line_formula(line) {
            return false;
        }
        // 同列小字(脚注/角标)
        let height = line_height.get(&line).copied().unwrap_or(0.0);
        let median_h = col_median_height[col];
        if column_lines[col].len() >= 4 && height < median_h * cal.small_height_ratio {
            return true;
        }
        // 单行窄文本(角标数字等)
        let width = (line_right.get(&line).copied().unwrap_or(0.0)
            - line_left.get(&line).copied().unwrap_or(0.0))
        .max(0.0);
        width < NARROW_LINE_WIDTH_CQW
    };

    let mut body = Vec::new();
    let mut small = Vec::new();
    for mut w in flat {
        if is_small_line(w.line) {
            w.formula = false;
            w.formula_score = 0.0;
            small.push(w);
        } else {
            let in_formula_col = column_of
                .get(&w.line)
                .map(|&c| formula_col(c))
                .unwrap_or(false);
            let score = column_of
                .get(&w.line)
                .map(|&c| formula_score_for(w.line, c))
                .unwrap_or(0.0);
            // 置信度是最终判据;line_formula 只通过分数参与,不再单独置位。
            w.formula = in_formula_col || score >= FORMULA_SCORE_THRESHOLD;
            w.formula_score = score;
            body.push(w);
        }
    }
    assign_gestures(&mut body);
    assign_gestures(&mut small);
    (body, small)
}

// ── 列工具(作用于某一层的 flat) ──

/// 行→列 与每列代表左缘(该列最小左缘)。
fn line_columns(flat: &[FlatWord]) -> (HashMap<usize, usize>, Vec<f64>) {
    line_columns_with(flat, crate::calibration::column_gap_cqw())
}

fn line_columns_with(flat: &[FlatWord], column_gap_cqw: f64) -> (HashMap<usize, usize>, Vec<f64>) {
    let mut line_left: HashMap<usize, f64> = HashMap::new();
    for w in flat {
        if w.text.trim().is_empty() {
            continue;
        }
        let e = line_left.entry(w.line).or_insert(f64::INFINITY);
        *e = (*e).min(w.left_cqw);
    }
    let mut ids: Vec<usize> = line_left.keys().copied().collect();
    ids.sort_by(|a, b| {
        line_left[a]
            .partial_cmp(&line_left[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut column_of: HashMap<usize, usize> = HashMap::new();
    let mut reps: Vec<f64> = Vec::new();
    let mut prev_left: Option<f64> = None;
    for lid in ids {
        // 按相邻行左缘间距分列,避免公式行因对齐逐渐右移被误拆成多列
        if prev_left.is_none() || line_left[&lid] - prev_left.unwrap() > column_gap_cqw {
            reps.push(line_left[&lid]);
        }
        column_of.insert(lid, reps.len() - 1);
        prev_left = Some(line_left[&lid]);
    }
    (column_of, reps)
}

// ── 选区矩形 / 复制 ──

/// 页面列布局:行→列 + 按左缘升序的代表左缘 + 每列尺寸统计。
///
/// `is_text_column` 用“正文尺寸”过滤图注/表格标签/碎片列,保证
/// 阅读顺序续接(左栏 → 右栏、跨页)只落在真正的正文列上。
struct PageLayout {
    column_of: HashMap<usize, usize>,
    reps: Vec<f64>,
    col_line_count: Vec<usize>,
    col_median_height: Vec<f64>,
    col_median_width: Vec<f64>,
    body_median_height: f64,
    body_median_width: f64,
    cal: crate::calibration::DocCalibration,
}

impl PageLayout {
    fn new_with(flat: &[FlatWord], cal: crate::calibration::DocCalibration) -> Self {
        let (column_of, reps) = line_columns_with(flat, cal.column_gap_cqw);

        let mut line_left: HashMap<usize, f64> = HashMap::new();
        let mut line_right: HashMap<usize, f64> = HashMap::new();
        let mut line_height: HashMap<usize, f64> = HashMap::new();
        for w in flat {
            if w.text.trim().is_empty() {
                continue;
            }
            let l = line_left.entry(w.line).or_insert(f64::INFINITY);
            *l = (*l).min(w.left_cqw);
            let r = line_right.entry(w.line).or_insert(f64::NEG_INFINITY);
            *r = (*r).max(w.left_cqw + w.width_cqw);
            let h = line_height.entry(w.line).or_insert(0.0);
            *h = (*h).max(w.line_height_cqw);
        }

        let col_count = reps.len();
        let mut col_lines: Vec<Vec<usize>> = vec![Vec::new(); col_count];
        for (l, &c) in &column_of {
            col_lines[c].push(*l);
        }
        let mut col_median_height = vec![0.0; col_count];
        let mut col_median_width = vec![0.0; col_count];
        let mut col_line_count = vec![0usize; col_count];
        for (c, lines) in col_lines.iter().enumerate() {
            col_line_count[c] = lines.len();
            let mut hs: Vec<f64> = lines
                .iter()
                .map(|l| line_height.get(l).copied().unwrap_or(0.0))
                .collect();
            hs.sort_by(|a, b| a.partial_cmp(b).unwrap());
            col_median_height[c] = median_sorted(&hs);
            let mut ws: Vec<f64> = lines
                .iter()
                .map(|l| (line_right[l] - line_left[l]).max(0.0))
                .collect();
            ws.sort_by(|a, b| a.partial_cmp(b).unwrap());
            col_median_width[c] = median_sorted(&ws);
        }

        // 正文中位宽度:≥4 行的列,再取其中较宽的一半,避免公式/图注列拉低。
        let upper_median = |vals: &mut Vec<f64>| -> f64 {
            vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
            let initial = median_sorted(vals);
            let upper: Vec<f64> = vals.iter().copied().filter(|v| *v >= initial).collect();
            if upper.is_empty() {
                initial
            } else {
                median_sorted(&upper)
            }
        };
        let mut cand_w: Vec<f64> = (0..col_count)
            .filter(|&c| col_line_count[c] >= 4)
            .map(|c| col_median_width[c])
            .collect();
        let body_median_width = if cand_w.is_empty() {
            let all: Vec<f64> = line_ids_widths(flat);
            median_sorted(&all)
        } else {
            upper_median(&mut cand_w)
        };
        // 正文中位高度:只统计“宽度达标(≥0.5×正文中位宽)”列里的行,
        // 避免公式列/竖排列的高字形拉高中位数,导致正文列被误判。
        let body_median_height = {
            let mut hs: Vec<f64> = Vec::new();
            for (c, lines) in col_lines.iter().enumerate() {
                if col_median_width[c] >= body_median_width.max(0.001) * 0.5 {
                    hs.extend(
                        lines
                            .iter()
                            .map(|l| line_height.get(l).copied().unwrap_or(0.0)),
                    );
                }
            }
            if hs.is_empty() {
                let mut all: Vec<f64> = line_height.values().copied().collect();
                all.sort_by(|a, b| a.partial_cmp(b).unwrap());
                median_sorted(&all)
            } else {
                hs.sort_by(|a, b| a.partial_cmp(b).unwrap());
                median_sorted(&hs)
            }
        };

        Self {
            column_of,
            reps,
            col_line_count,
            col_median_height,
            col_median_width,
            body_median_height,
            body_median_width,
            cal,
        }
    }

    fn col_of(&self, flat: &[FlatWord], idx: usize) -> Option<usize> {
        self.column_of.get(&flat[idx].line).copied()
    }

    fn rep(&self, col: usize) -> f64 {
        self.reps[col]
    }

    /// 某列的词在 flat 中的索引(内容顺序,含空格词)。
    fn word_indices(&self, flat: &[FlatWord], col: usize) -> Vec<usize> {
        flat.iter()
            .enumerate()
            .filter(|(_, w)| self.column_of.get(&w.line) == Some(&col))
            .map(|(i, _)| i)
            .collect()
    }

    /// 内容顺序中位于 col 之后的下一个列(即 col 最后一个词后面那个词所属的列)。
    fn next_column_in_content_order(&self, flat: &[FlatWord], col: usize) -> Option<usize> {
        let words = self.word_indices(flat, col);
        let last = *words.last()?;
        for w in flat.iter().skip(last + 1) {
            if let Some(&c) = self.column_of.get(&w.line) {
                if c != col {
                    return Some(c);
                }
            }
        }
        None
    }

    /// 内容顺序中 col 之后的下一个“正文列”(跳过图注/表格标签/碎片列)。
    fn next_text_column(&self, flat: &[FlatWord], col: usize) -> Option<usize> {
        let mut cursor = col;
        loop {
            let next = self.next_column_in_content_order(flat, cursor)?;
            if self.is_text_column(next) {
                return Some(next);
            }
            cursor = next;
        }
    }

    /// 内容顺序中的所有正文列。
    fn text_columns_in_content_order(&self, flat: &[FlatWord]) -> Vec<usize> {
        let mut seen = std::collections::HashSet::new();
        let mut cols = Vec::new();
        for w in flat {
            if w.text.trim().is_empty() {
                continue;
            }
            if let Some(&c) = self.column_of.get(&w.line) {
                if seen.insert(c) && self.is_text_column(c) {
                    cols.push(c);
                }
            }
        }
        cols
    }

    /// 内容顺序中 col 之前的正文列(用于右栏 → 左栏的反向续接)。
    fn previous_text_column(&self, flat: &[FlatWord], col: usize) -> Option<usize> {
        let cols = self.text_columns_in_content_order(flat);
        let pos = cols.iter().position(|&c| c == col)?;
        if pos == 0 { None } else { Some(cols[pos - 1]) }
    }

    /// 是否为正文列:≥2 个有字的行,且行高 ≥ 0.85×正文中位高、
    /// 行宽 ≥ 0.5×正文中位宽(用于跳过图注/表格标签等小字列)。
    fn is_text_column(&self, col: usize) -> bool {
        if col >= self.col_line_count.len() || self.col_line_count[col] < 2 {
            return false;
        }
        let mh = self.col_median_height[col];
        let mw = self.col_median_width[col];
        mh >= self.body_median_height.max(0.001) * 0.85
            && mw >= self.body_median_width.max(0.001) * 0.5
    }
}

fn line_ids_widths(flat: &[FlatWord]) -> Vec<f64> {
    let mut line_left: HashMap<usize, f64> = HashMap::new();
    let mut line_right: HashMap<usize, f64> = HashMap::new();
    for w in flat {
        if w.text.trim().is_empty() {
            continue;
        }
        let l = line_left.entry(w.line).or_insert(f64::INFINITY);
        *l = (*l).min(w.left_cqw);
        let r = line_right.entry(w.line).or_insert(f64::NEG_INFINITY);
        *r = (*r).max(w.left_cqw + w.width_cqw);
    }
    let mut ws: Vec<f64> = line_left
        .keys()
        .map(|l| (line_right[l] - line_left[l]).max(0.0))
        .collect();
    ws.sort_by(|a, b| a.partial_cmp(b).unwrap());
    ws
}

/// 高亮矩形;col=Some 时跳过其他列的词。
fn selection_rects_filtered(
    flat: &[FlatWord],
    lo: usize,
    hi: usize,
    col: Option<f64>,
) -> Vec<SelectionRect> {
    let (column_of, reps) = line_columns(flat);
    let same_col = |i: usize| -> bool {
        match col {
            None => true,
            Some(c) => column_of
                .get(&flat[i].line)
                .and_then(|&x| reps.get(x))
                .map(|r| (r - c).abs() < 1.0)
                .unwrap_or(false),
        }
    };

    let mut rects = Vec::new();
    let mut i = lo;
    while i <= hi {
        let line = flat[i].line;
        let mut left = f64::INFINITY;
        let mut right = f64::NEG_INFINITY;
        let mut top = f64::INFINITY;
        let mut height: f64 = 0.0;
        let mut j = i;
        while j <= hi && flat[j].line == line {
            if !flat[j].text.trim().is_empty() && same_col(j) {
                left = left.min(flat[j].left_cqw);
                right = right.max(flat[j].left_cqw + flat[j].width_cqw);
                top = top.min(flat[j].top_cqw);
                height = height.max(flat[j].line_height_cqw);
            }
            j += 1;
        }
        if left.is_finite() {
            rects.push(SelectionRect {
                left_cqw: left,
                top_cqw: top,
                width_cqw: (right - left).max(0.01),
                height_cqw: height,
            });
        }
        i = j;
    }
    rects
}

/// 拼接选中文本;col=Some 时跳过其他列的词;行尾空格不复制。
fn copy_text_filtered(flat: &[FlatWord], lo: usize, hi: usize, col: Option<f64>) -> String {
    if flat.is_empty() || lo > hi {
        return String::new();
    }
    let hi = hi.min(flat.len() - 1);
    let (column_of, reps) = line_columns(flat);
    let same_col = |i: usize| -> bool {
        match col {
            None => true,
            Some(c) => column_of
                .get(&flat[i].line)
                .and_then(|&x| reps.get(x))
                .map(|r| (r - c).abs() < 1.0)
                .unwrap_or(false),
        }
    };

    let mut out = String::new();
    let mut i = lo;
    while i <= hi {
        if !same_col(i) {
            i += 1;
            continue;
        }
        let mut next = i + 1;
        while next <= hi && !same_col(next) {
            next += 1;
        }
        let is_line_end = next > hi || flat[next].line != flat[i].line;
        if !(flat[i].text.trim().is_empty() && is_line_end) {
            out.push_str(&flat[i].text);
        }
        if next <= hi && flat[next].line != flat[i].line {
            out.push('\n');
        }
        i = next;
    }
    out
}

/// 按阅读顺序复制选区:每步一段,步骤之间用换行拼接。
fn copy_steps<'a>(
    sel: &Selection,
    flat_of: impl Fn(u32) -> Option<&'a [FlatWord]>,
) -> Option<String> {
    let mut parts = Vec::new();
    for step in &sel.steps {
        let flat = flat_of(step.page)?;
        parts.push(copy_text_filtered(flat, step.lo, step.hi, step.column_left));
    }
    Some(parts.join("\n"))
}

/// 按阅读顺序抽取翻译输入:正文保留,连续公式词(含其间空格)合并为
/// 一个 `[公式N]` 占位符,内容为该段公式的文本层原文。
fn selection_translation_input<'a>(
    sel: &Selection,
    flat_of: impl Fn(u32) -> Option<&'a [FlatWord]>,
) -> Option<(String, Vec<String>)> {
    let mut parts = Vec::new();
    let mut formulas = Vec::new();
    for step in &sel.steps {
        let flat = flat_of(step.page)?;
        parts.push(translation_step_text(
            flat,
            step.lo,
            step.hi,
            step.column_left,
            &mut formulas,
        ));
    }
    Some((parts.join("\n"), formulas))
}

/// 单步翻译文本:与 copy_text_filtered 同款列过滤与换行语义,
/// 公式词运行(含其间的空格词)折叠成占位符。
fn translation_step_text(
    flat: &[FlatWord],
    lo: usize,
    hi: usize,
    col: Option<f64>,
    formulas: &mut Vec<String>,
) -> String {
    if flat.is_empty() || lo > hi {
        return String::new();
    }
    let hi = hi.min(flat.len() - 1);
    let (column_of, reps) = line_columns(flat);
    let same_col = |i: usize| -> bool {
        match col {
            None => true,
            Some(c) => column_of
                .get(&flat[i].line)
                .and_then(|&x| reps.get(x))
                .map(|r| (r - c).abs() < 1.0)
                .unwrap_or(false),
        }
    };

    let mut out = String::new();
    let mut i = lo;
    while i <= hi {
        if !same_col(i) {
            i += 1;
            continue;
        }
        if flat[i].formula {
            // 连续公式词合并;空格词仅在后面仍跟公式词时纳入
            let mut raw = String::new();
            let mut last_line = flat[i].line;
            let mut j = i;
            while j <= hi {
                let w = &flat[j];
                if !same_col(j) {
                    break;
                }
                if w.formula {
                    if w.line != last_line {
                        raw.push('\n');
                        last_line = w.line;
                    }
                    raw.push_str(&w.text);
                    j += 1;
                } else if w.text.trim().is_empty() {
                    let mut k = j + 1;
                    let mut has_formula_after = false;
                    while k <= hi && same_col(k) {
                        if flat[k].formula {
                            has_formula_after = true;
                            break;
                        }
                        if !flat[k].text.trim().is_empty() {
                            break;
                        }
                        k += 1;
                    }
                    if has_formula_after {
                        if w.line != last_line {
                            raw.push('\n');
                            last_line = w.line;
                        }
                        raw.push_str(&w.text);
                        j += 1;
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            let trimmed = raw.trim().to_string();
            if !trimmed.is_empty() {
                formulas.push(trimmed);
                out.push_str(&format!("[公式{}]", formulas.len()));
            } else {
                out.push_str(&flat[i].text);
                j = i + 1;
            }
            if j <= hi && same_col(j) && flat[j].line != last_line {
                out.push('\n');
            }
            i = j;
            continue;
        }

        let mut next = i + 1;
        while next <= hi && !same_col(next) {
            next += 1;
        }
        let is_line_end = next > hi || flat[next].line != flat[i].line;
        if !(flat[i].text.trim().is_empty() && is_line_end) {
            out.push_str(&flat[i].text);
        }
        if next <= hi && flat[next].line != flat[i].line {
            out.push('\n');
        }
        i = next;
    }
    out
}

/// 选区中是否存在非空格正文词(用于决定是否显示翻译按钮)。
fn selection_has_plain_text<'a>(
    sel: &Selection,
    flat_of: impl Fn(u32) -> Option<&'a [FlatWord]>,
) -> bool {
    for step in &sel.steps {
        let Some(flat) = flat_of(step.page) else {
            continue;
        };
        let hi = step.hi.min(flat.len().saturating_sub(1));
        if step.lo > hi {
            continue;
        }
        for w in &flat[step.lo..=hi] {
            if !w.formula && !w.text.trim().is_empty() {
                return true;
            }
        }
    }
    false
}

/// 把索引吸附到最近的非空格词。
fn snap_to_word(flat: &[FlatWord], idx: usize) -> usize {
    if flat.is_empty() {
        return 0;
    }
    let idx = idx.min(flat.len() - 1);
    if !flat[idx].text.trim().is_empty() {
        return idx;
    }
    let mut i = idx;
    while i > 0 && flat[i].text.trim().is_empty() {
        i -= 1;
    }
    if !flat[i].text.trim().is_empty() {
        return i;
    }
    let mut j = idx;
    while j + 1 < flat.len() && flat[j].text.trim().is_empty() {
        j += 1;
    }
    j
}

/// 通过 macOS 系统剪贴板工具 pbcopy 写入。
fn copy_to_clipboard(text: &str) -> bool {
    let Ok(mut child) = Command::new("/usr/bin/pbcopy")
        .stdin(Stdio::piped())
        .spawn()
    else {
        return false;
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(text.as_bytes());
    }
    child.wait().map(|s| s.success()).unwrap_or(false)
}

fn copy_feedback(error_signal: Signal<ErrorSignal>, ok: bool) {
    let mut err = error_signal;
    err.write().push(ErrorInfo::new(
        "reader-copy",
        if ok { "已复制" } else { "复制失败" },
        if ok {
            "选中内容已复制到剪贴板"
        } else {
            "无法写入系统剪贴板"
        },
        if ok {
            ErrorSeverity::Info
        } else {
            ErrorSeverity::Warning
        },
        ErrorSource::General,
    ));
}

/// 把选区矩形换算为 @OCR_RENDER_SCALE 像素坐标的裁剪框。
fn selection_bbox_px(
    flat: &[FlatWord],
    step: &SelectionStep,
    page_width_pt: f32,
) -> (i32, i32, i32, i32) {
    let rects = selection_rects_filtered(flat, step.lo, step.hi, step.column_left);
    if rects.is_empty() {
        return (0, 0, 1, 1);
    }
    let left = rects
        .iter()
        .map(|r| r.left_cqw)
        .fold(f64::INFINITY, f64::min);
    let top = rects
        .iter()
        .map(|r| r.top_cqw)
        .fold(f64::INFINITY, f64::min);
    let right = rects
        .iter()
        .map(|r| r.left_cqw + r.width_cqw)
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = rects
        .iter()
        .map(|r| r.top_cqw + r.height_cqw)
        .fold(f64::NEG_INFINITY, f64::max);
    let scale = page_width_pt as f64 * OCR_RENDER_SCALE as f64;
    (
        ((left / 100.0) * scale).floor().max(0.0) as i32,
        ((top / 100.0) * scale).floor().max(0.0) as i32,
        (((right - left) / 100.0) * scale).ceil().max(1.0) as i32,
        (((bottom - top) / 100.0) * scale).ceil().max(1.0) as i32,
    )
}

/// 单槽缓存键:同一 (book, page, 裁剪框) 复用同一结果。
fn formula_copy_key(book_id: &str, page: u32, bbox: (i32, i32, i32, i32)) -> String {
    format!(
        "{}:{}:{}:{}:{}:{}@{}",
        book_id, page, bbox.0, bbox.1, bbox.2, bbox.3, OCR_RENDER_SCALE
    )
}

/// 在阻塞线程中执行:4× 重渲染 → 裁剪 → 白底 → OCR。
fn run_formula_ocr(
    doc: Arc<PdfDocument>,
    page: usize,
    bbox: (i32, i32, i32, i32),
) -> Result<String, crate::formula_ocr::FormulaOcrError> {
    let png = doc
        .render_page_png(page as u32, OCR_RENDER_SCALE)
        .map_err(|e| crate::formula_ocr::FormulaOcrError::Io(format!("渲染页面失败:{e:#}")))?;
    let img = image::load_from_memory(&png)
        .map_err(|e| crate::formula_ocr::FormulaOcrError::Decode(format!("解码页面图像失败:{e}")))?
        .to_rgba8();
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
    let crop = image::imageops::crop_imm(&img, x as u32, y as u32, w as u32, h as u32).to_image();
    image::imageops::overlay(&mut canvas, &crop, PAD as i64, PAD as i64);
    let backend = crate::formula_ocr::backend_arc()?;
    backend.recognize_rgba(canvas.as_raw(), canvas.width(), canvas.height())
}

/// 单 worker 串行处理公式复制;处理中收到新请求则覆盖 pending,
/// 旧结果返回时若已不是最新请求则丢弃;单槽缓存只保留最近一次结果。
fn spawn_copy_worker(
    mut session: Signal<Option<ReaderSession>>,
    error_signal: Signal<ErrorSignal>,
) {
    spawn(async move {
        loop {
            let req = {
                let mut guard = session.write();
                let Some(inner) = guard.as_mut() else {
                    break;
                };
                inner.copy_busy = true;
                match inner.pending_copy.take() {
                    Some(r) => r,
                    None => {
                        inner.copy_busy = false;
                        break;
                    }
                }
            };
            let result = tokio::task::spawn_blocking({
                let doc = req.doc.clone();
                let page = req.page;
                let bbox = req.bbox;
                move || run_formula_ocr(doc, page, bbox)
            })
            .await;
            let outcome = match result {
                Ok(r) => r,
                Err(e) => Err(crate::formula_ocr::FormulaOcrError::Io(format!(
                    "OCR 任务失败:{e}"
                ))),
            };

            let mut guard = session.write();
            let Some(inner) = guard.as_mut() else {
                break;
            };
            if inner.pending_copy.is_some() {
                continue; // 已有更新请求,丢弃本次结果
            }
            inner.copy_busy = false;
            let mut success_gen = 0u64;
            match outcome {
                Ok(latex) => {
                    let _ = copy_to_clipboard(&latex);
                    inner.ocr_cache.put(req.key, latex);
                    if let Some(bar) = inner.action_bar.as_mut() {
                        bar.status = ActionBarStatus::Success;
                        success_gen = bar.generation;
                    }
                    let mut err = error_signal;
                    err.write().push(ErrorInfo::new(
                        "reader-copy",
                        "已复制 LaTeX",
                        "公式已识别并复制到剪贴板",
                        ErrorSeverity::Info,
                        ErrorSource::General,
                    ));
                }
                Err(e) => {
                    let _ = copy_to_clipboard(&req.fallback);
                    if let Some(bar) = inner.action_bar.as_mut() {
                        bar.status = ActionBarStatus::Error;
                    }
                    // 未配置 OCR 时静默回退文本层,不弹报警;
                    // 真正的推理/IO 错误仍提示。
                    if !matches!(&e, crate::formula_ocr::FormulaOcrError::NotConfigured(_)) {
                        let mut err = error_signal;
                        err.write().push(ErrorInfo::new(
                            "reader-copy",
                            "OCR 不可用,已复制文本层重建结果",
                            format!("{e}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                }
            }
            drop(guard);
            if success_gen != 0 {
                schedule_action_bar_reset(session, success_gen);
            }
        }
    });
}

// ── 句子/段落范围(本层内、同列过滤) ──

/// 扫描一个词:维护括号深度;深度 0 处的 `;`/`；` 或词尾 `. ! ? 。！？`
/// 都视为句子边界(全局规则,正文与公式一致)。
fn scan_word_boundary(text: &str, depth: &mut usize, next: Option<&str>) -> bool {
    for c in text.chars() {
        match c {
            '(' | '[' | '{' => *depth += 1,
            ')' | ']' | '}' => *depth = depth.saturating_sub(1),
            ';' | '；' if *depth == 0 => return true,
            _ => {}
        }
    }
    let t = text.trim_end();
    (t.ends_with('.')
        || t.ends_with('!')
        || t.ends_with('?')
        || t.ends_with('。')
        || t.ends_with('！')
        || t.ends_with('？'))
        && !is_citation_period(text, next)
}

/// 引文/缩写里的句点(如 `al.`、`e.g.`、`et al.`)后面紧跟 `, ] )` 或数字时,
/// 不是句子结束,只是引文中间。
fn is_citation_period(word: &str, next: Option<&str>) -> bool {
    let t = word.trim_end();
    if !t.ends_with('.') {
        return false;
    }
    let stem = &t[..t.len() - 1];
    let abbrev = matches!(
        stem,
        "al" | "e.g"
            | "i.e"
            | "etc"
            | "vs"
            | "et al"
            | "fig"
            | "figs"
            | "sec"
            | "secs"
            | "no"
            | "pp"
    );
    if !abbrev {
        return false;
    }
    match next.and_then(|s| s.trim_start().chars().next()) {
        Some(',') | Some(']') | Some(')') => true,
        Some(c) if c.is_ascii_digit() => true,
        _ => false,
    }
}

/// flat 中 from 之后第一个非空格词的文本。
fn next_non_space(flat: &[FlatWord], from: usize) -> Option<&str> {
    flat.iter()
        .skip(from)
        .find(|w| !w.text.trim().is_empty())
        .map(|w| w.text.as_str())
}

/// 行是否带公式标记(取该行第一个非空格词)。
fn line_is_formula(flat: &[FlatWord], line: usize) -> bool {
    flat.iter()
        .find(|w| w.line == line && !w.text.trim().is_empty())
        .map(|w| w.formula)
        .unwrap_or(false)
}

/// 公式句:只在锚点列内,以行尾 `;`/句读或垂直间隙断块,不跨栏/跨页。
#[cfg(test)]
fn formula_sentence_walk(flat: &[FlatWord], idx: usize, start_page: u32) -> Vec<SelectionStep> {
    formula_sentence_walk_with(flat, idx, start_page, crate::calibration::current())
}

fn formula_sentence_walk_with(
    flat: &[FlatWord],
    idx: usize,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    if flat.is_empty() {
        return Vec::new();
    }
    let idx = snap_to_word(flat, idx).min(flat.len() - 1);
    let layout = PageLayout::new_with(flat, cal);
    let Some(anchor_col) = layout.col_of(flat, idx) else {
        return Vec::new();
    };
    let rep = layout.rep(anchor_col);
    let col_words = layout.word_indices(flat, anchor_col);
    if col_words.is_empty() {
        return Vec::new();
    }
    let target_line = flat[idx].line;

    let mut line_top: HashMap<usize, f64> = HashMap::new();
    let mut line_height: HashMap<usize, f64> = HashMap::new();
    for w in flat {
        if w.text.trim().is_empty() {
            continue;
        }
        let e = line_top.entry(w.line).or_insert(f64::INFINITY);
        *e = (*e).min(w.top_cqw);
        line_height.entry(w.line).or_insert(w.line_height_cqw);
    }
    let mut col_lines: Vec<usize> = col_words.iter().map(|&i| flat[i].line).collect();
    col_lines.sort_by(|a, b| {
        line_top[a]
            .partial_cmp(&line_top[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    col_lines.dedup();
    let Some(pos) = col_lines.iter().position(|&l| l == target_line) else {
        return Vec::new();
    };

    let adjacent = |a: usize, b: usize| -> bool {
        let gap = line_top[&b] - (line_top[&a] + line_height[&a]);
        gap <= line_height[&a].max(line_height[&b]).max(0.6) * cal.vertical_gap_ratio
    };
    let mut run_lo = pos;
    while run_lo > 0
        && adjacent(col_lines[run_lo - 1], col_lines[run_lo])
        && line_is_formula(flat, col_lines[run_lo - 1])
    {
        run_lo -= 1;
    }
    let mut run_hi = pos;
    while run_hi + 1 < col_lines.len()
        && adjacent(col_lines[run_hi], col_lines[run_hi + 1])
        && line_is_formula(flat, col_lines[run_hi + 1])
    {
        run_hi += 1;
    }
    let run_lines: HashSet<usize> = col_lines[run_lo..=run_hi].iter().copied().collect();
    let run_words: Vec<usize> = col_words
        .iter()
        .copied()
        .filter(|&wi| run_lines.contains(&flat[wi].line))
        .collect();
    if run_words.is_empty() {
        return Vec::new();
    }

    let pos_in_run = run_words.iter().position(|&i| i == idx).unwrap_or(0);
    let mut depth = 0usize;
    let mut start_pos = 0usize;
    for k in 0..pos_in_run {
        if scan_word_boundary(
            &flat[run_words[k]].text,
            &mut depth,
            run_words.get(k + 1).map(|&i| flat[i].text.as_str()),
        ) {
            start_pos = k + 1;
        }
    }
    let mut end_pos = run_words.len() - 1;
    for k in start_pos..run_words.len() {
        if scan_word_boundary(
            &flat[run_words[k]].text,
            &mut depth,
            run_words.get(k + 1).map(|&i| flat[i].text.as_str()),
        ) {
            end_pos = k;
            break;
        }
    }
    vec![SelectionStep {
        page: start_page,
        lo: run_words[start_pos],
        hi: run_words[end_pos],
        column_left: Some(rep),
    }]
}

/// 按阅读顺序选择句子:可向前续接到上一正文列/上一页(右栏 → 左栏),
/// 也可向后续接到下一正文列/下一页(左栏 → 右栏),最多各跨 1 页。
fn sentence_walk(
    flat: &[FlatWord],
    idx: usize,
    prev_flat: Option<&[FlatWord]>,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
) -> Vec<SelectionStep> {
    sentence_walk_with(
        flat,
        idx,
        prev_flat,
        next_flat,
        start_page,
        crate::calibration::current(),
    )
}

fn sentence_walk_with(
    flat: &[FlatWord],
    idx: usize,
    prev_flat: Option<&[FlatWord]>,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    if flat.is_empty() {
        return Vec::new();
    }
    let idx = snap_to_word(flat, idx).min(flat.len() - 1);
    if flat[idx].formula {
        return formula_sentence_walk_with(flat, idx, start_page, cal);
    }
    let mut steps = sentence_backward_with(flat, idx, prev_flat, start_page, cal);
    steps.extend(sentence_forward_with(flat, idx, next_flat, start_page, cal));
    steps
}

/// 从锚点句向前(右栏 → 左栏 → 上一页)补全句子前半部分。
fn sentence_backward_with(
    flat: &[FlatWord],
    idx: usize,
    prev_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    let layout = PageLayout::new_with(flat, cal);
    let Some(col) = layout.col_of(flat, idx) else {
        return Vec::new();
    };
    let col_words = layout.word_indices(flat, col);
    if col_words.is_empty() {
        return Vec::new();
    }
    let pos = col_words.iter().position(|&i| i == idx).unwrap_or(0);

    // 锚点句必须从该列第一词开始(列内前面没有边界)
    let mut depth = 0usize;
    let mut start_pos = 0usize;
    for k in 0..pos {
        if scan_word_boundary(
            &flat[col_words[k]].text,
            &mut depth,
            col_words.get(k + 1).map(|&i| flat[i].text.as_str()),
        ) {
            start_pos = k + 1;
        }
    }
    if start_pos != 0 {
        return Vec::new();
    }
    let mut d0 = 0usize;
    if scan_word_boundary(
        &flat[col_words[0]].text,
        &mut d0,
        col_words.get(1).map(|&i| flat[i].text.as_str()),
    ) {
        return Vec::new(); // 句子从列首词开始,不向前续
    }

    // 同页前一正文列
    if let Some(prev_col) = layout.previous_text_column(flat, col) {
        let prev_words = layout.word_indices(flat, prev_col);
        let Some((lo, hi)) = sentence_tail_in_column(flat, &prev_words) else {
            return Vec::new();
        };
        let mut steps = Vec::new();
        // 若整个前一列都没有边界,继续向前递归
        if lo == prev_words[0] {
            steps = sentence_backward_with(flat, prev_words[0], prev_flat, start_page, cal);
        }
        steps.push(SelectionStep {
            page: start_page,
            lo,
            hi,
            column_left: Some(layout.rep(prev_col)),
        });
        return steps;
    }

    // 上一页最后正文列(最多 1 页)
    if let Some(prev) = prev_flat {
        let prev_layout = PageLayout::new_with(prev, cal);
        if let Some(&prev_col) = prev_layout.text_columns_in_content_order(prev).last() {
            let prev_words = prev_layout.word_indices(prev, prev_col);
            if let Some((lo, hi)) = sentence_tail_in_column(prev, &prev_words) {
                return vec![SelectionStep {
                    page: start_page.saturating_sub(1),
                    lo,
                    hi,
                    column_left: Some(prev_layout.rep(prev_col)),
                }];
            }
        }
    }
    Vec::new()
}

/// 该列中“未结束句子的尾部”:最后一个边界之后的词段;末词本身是边界则 None。
fn sentence_tail_in_column(flat: &[FlatWord], words: &[usize]) -> Option<(usize, usize)> {
    if words.is_empty() {
        return None;
    }
    let mut depth = 0usize;
    let mut last_boundary: Option<usize> = None;
    for (k, &wi) in words.iter().enumerate() {
        if scan_word_boundary(
            &flat[wi].text,
            &mut depth,
            words.get(k + 1).map(|&i| flat[i].text.as_str()),
        ) {
            last_boundary = Some(k);
        }
    }
    match last_boundary {
        None => Some((words[0], *words.last().unwrap())),
        Some(b) if b == words.len() - 1 => None,
        Some(b) => Some((words[b + 1], *words.last().unwrap())),
    }
}

/// 句子主体:先在锚点列内找边界,未结束则按内容顺序向后续接
/// (跳过图注/表格标签列,跨到下一个正文列自动切步;本页耗尽后续下一页)。
fn sentence_forward_with(
    flat: &[FlatWord],
    idx: usize,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    let mut steps = Vec::new();
    if flat.is_empty() {
        return steps;
    }
    let idx = snap_to_word(flat, idx).min(flat.len() - 1);
    let layout = PageLayout::new_with(flat, cal);
    let Some(anchor_col) = layout.col_of(flat, idx) else {
        return steps;
    };
    let anchor_rep = layout.rep(anchor_col);
    let col_words = layout.word_indices(flat, anchor_col);
    if col_words.is_empty() {
        return steps;
    }
    let pos_in_col = col_words.iter().position(|&i| i == idx).unwrap_or(0);

    // 起点:锚点之前最近边界之后,同时记录该位置的括号深度
    let mut depth = 0usize;
    let mut start_pos = 0usize;
    for k in 0..pos_in_col {
        if scan_word_boundary(
            &flat[col_words[k]].text,
            &mut depth,
            col_words.get(k + 1).map(|&i| flat[i].text.as_str()),
        ) {
            start_pos = k + 1;
        }
    }

    // 锚点列内向前找边界(含锚点本身)
    let mut term: Option<usize> = None;
    for k in start_pos..col_words.len() {
        if scan_word_boundary(
            &flat[col_words[k]].text,
            &mut depth,
            col_words.get(k + 1).map(|&i| flat[i].text.as_str()),
        ) {
            term = Some(col_words[k]);
            break;
        }
    }
    if let Some(t) = term {
        steps.push(SelectionStep {
            page: start_page,
            lo: col_words[start_pos],
            hi: t,
            column_left: Some(anchor_rep),
        });
        return steps;
    }
    let last = *col_words.last().unwrap();
    steps.push(SelectionStep {
        page: start_page,
        lo: col_words[start_pos],
        hi: last,
        column_left: Some(anchor_rep),
    });

    // 从锚点列末词之后按内容顺序续接(括号深度跨列/跨页延续)
    let mut page = start_page;
    let mut current_flat = flat;
    let from = last + 1;
    let mut crossed_page = false;
    loop {
        let layout = PageLayout::new_with(current_flat, cal);
        let mut acc: Option<(usize, usize, usize)> = None; // (lo, hi, col)
        let mut j;
        if page == start_page {
            j = from;
        } else {
            // 跨页续接:跳到下一页第一个正文列(与段落逻辑一致),
            // 跳过页首的表格/公式等非正文内容
            let Some(ncol) = (0..layout.reps.len()).find(|&c| layout.is_text_column(c)) else {
                break;
            };
            if column_first_line_indented(current_flat, &layout, ncol) {
                break;
            }
            let Some(&first) = layout.word_indices(current_flat, ncol).first() else {
                break;
            };
            j = first;
        }
        while j < current_flat.len() {
            let Some(c) = layout.col_of(current_flat, j) else {
                j += 1;
                continue;
            };
            // 跳过图注/表格标签等非正文列,但公式词始终参与句子流
            // (公式行靠句号/分号自然断句,不会吞掉整张表格)
            if !layout.is_text_column(c) && !current_flat[j].formula {
                j += 1;
                continue;
            }
            match acc {
                None => acc = Some((j, j, c)),
                Some((lo, hi, sc)) if sc != c => {
                    steps.push(SelectionStep {
                        page,
                        lo,
                        hi,
                        column_left: Some(layout.rep(sc)),
                    });
                    acc = Some((j, j, c));
                }
                Some((lo, _, sc)) => acc = Some((lo, j, sc)),
            }
            if scan_word_boundary(
                &current_flat[j].text,
                &mut depth,
                next_non_space(current_flat, j + 1),
            ) {
                if let Some((lo, _, c)) = acc {
                    steps.push(SelectionStep {
                        page,
                        lo,
                        hi: j,
                        column_left: Some(layout.rep(c)),
                    });
                }
                return steps;
            }
            j += 1;
        }
        if let Some((lo, hi, c)) = acc {
            steps.push(SelectionStep {
                page,
                lo,
                hi,
                column_left: Some(layout.rep(c)),
            });
        }
        if crossed_page {
            break;
        }
        if let Some(next) = next_flat {
            page += 1;
            current_flat = next;
            crossed_page = true;
            continue;
        }
        break;
    }
    steps
}

/// 词 idx 所在段落在 flat 中的范围(同列;行距/缩进断段)。
#[cfg(test)]
fn paragraph_range(flat: &[FlatWord], idx: usize) -> (usize, usize) {
    paragraph_range_with(flat, idx, crate::calibration::current())
}

fn paragraph_range_with(
    flat: &[FlatWord],
    idx: usize,
    cal: crate::calibration::DocCalibration,
) -> (usize, usize) {
    if flat.is_empty() {
        return (0, 0);
    }
    let idx = snap_to_word(flat, idx);
    let idx = idx.min(flat.len() - 1);
    let target_line = flat[idx].line;

    let mut line_top: HashMap<usize, f64> = HashMap::new();
    let mut line_height: HashMap<usize, f64> = HashMap::new();
    let mut line_left: HashMap<usize, f64> = HashMap::new();
    for w in flat {
        if w.text.trim().is_empty() {
            continue;
        }
        let e = line_top.entry(w.line).or_insert(f64::INFINITY);
        *e = (*e).min(w.top_cqw);
        line_height.entry(w.line).or_insert(w.line_height_cqw);
        let l = line_left.entry(w.line).or_insert(f64::INFINITY);
        *l = (*l).min(w.left_cqw);
    }

    let (column_of, _) = line_columns_with(flat, cal.column_gap_cqw);
    let Some(target_col) = column_of.get(&target_line).copied() else {
        return (0, flat.len() - 1);
    };
    let mut col_lines: Vec<usize> = line_top.keys().copied().collect();
    col_lines.sort_by(|a, b| {
        line_top[a]
            .partial_cmp(&line_top[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    col_lines.retain(|l| column_of.get(l) == Some(&target_col));
    let Some(pos) = col_lines.iter().position(|&l| l == target_line) else {
        return (0, flat.len() - 1);
    };

    let gap_ok = |a: usize, b: usize| -> bool {
        let gap = line_top[&b] - (line_top[&a] + line_height[&a]);
        let threshold = line_height[&a].max(line_height[&b]).max(0.6);
        if gap > threshold {
            return false;
        }
        let indent = line_left[&b] - line_left[&a];
        // 句子没结束(上一行末无句读)时,缩进只是排版延续,不算新段落;
        // 句子结束后出现缩进才视为另起一段。
        indent <= cal.paragraph_indent_cqw || !line_ends_sentence(flat, a)
    };

    let mut start_line = target_line;
    let mut i = pos;
    while i > 0 && gap_ok(col_lines[i - 1], col_lines[i]) {
        i -= 1;
        start_line = col_lines[i];
    }
    let mut end_line = target_line;
    let mut j = pos;
    while j + 1 < col_lines.len() && gap_ok(col_lines[j], col_lines[j + 1]) {
        j += 1;
        end_line = col_lines[j];
    }

    let start = flat.iter().position(|w| w.line == start_line).unwrap_or(0);
    let end = flat
        .iter()
        .rposition(|w| w.line == end_line)
        .unwrap_or(flat.len() - 1);
    (start, end)
}

/// 该行最后一个非空格词是否以句读结束(决定下一行缩进是否算新段落)。
fn line_ends_sentence(flat: &[FlatWord], line: usize) -> bool {
    flat.iter()
        .rev()
        .find(|w| w.line == line && !w.text.trim().is_empty())
        .map(|w| {
            let t = w.text.trim_end();
            t.ends_with('.')
                || t.ends_with('!')
                || t.ends_with('?')
                || t.ends_with('。')
                || t.ends_with('！')
                || t.ends_with('？')
                || t.ends_with(';')
                || t.ends_with('；')
        })
        .unwrap_or(false)
}

/// 某列首行是否缩进(左缘明显大于列代表左缘)。
fn column_first_line_indented(flat: &[FlatWord], layout: &PageLayout, col: usize) -> bool {
    let rep = layout.rep(col);
    let mut top_line: Option<usize> = None;
    let mut min_top = f64::INFINITY;
    let mut line_left: HashMap<usize, f64> = HashMap::new();
    for w in flat {
        if layout.column_of.get(&w.line) != Some(&col) || w.text.trim().is_empty() {
            continue;
        }
        let l = line_left.entry(w.line).or_insert(f64::INFINITY);
        *l = (*l).min(w.left_cqw);
        if w.top_cqw < min_top {
            min_top = w.top_cqw;
            top_line = Some(w.line);
        }
    }
    top_line
        .and_then(|l| line_left.get(&l))
        .map(|l| l - rep > layout.cal.paragraph_indent_cqw)
        .unwrap_or(false)
}

/// 公式块:Cmd+双击时,选中同一水平行带内、同一列垂直连续的整组公式
/// (含 `p`/`∗` 等碎片),不跨表格行。
#[cfg(test)]
fn formula_block_range(flat: &[FlatWord], idx: usize) -> (usize, usize) {
    formula_block_range_with(flat, idx, crate::calibration::current())
}

fn formula_block_range_with(
    flat: &[FlatWord],
    idx: usize,
    cal: crate::calibration::DocCalibration,
) -> (usize, usize) {
    if flat.is_empty() {
        return (0, 0);
    }
    let idx = snap_to_word(flat, idx).min(flat.len() - 1);
    let target_line = flat[idx].line;

    let mut line_top: HashMap<usize, f64> = HashMap::new();
    let mut line_height: HashMap<usize, f64> = HashMap::new();
    for w in flat {
        if w.text.trim().is_empty() {
            continue;
        }
        let e = line_top.entry(w.line).or_insert(f64::INFINITY);
        *e = (*e).min(w.top_cqw);
        line_height.entry(w.line).or_insert(w.line_height_cqw);
    }

    // 页面级水平行带:垂直间隙 > 校准比例 × 行高即新带
    let mut all_lines: Vec<usize> = line_top.keys().copied().collect();
    all_lines.sort_by(|a, b| {
        line_top[a]
            .partial_cmp(&line_top[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut band_of: HashMap<usize, usize> = HashMap::new();
    let mut band = 0usize;
    let mut prev: Option<usize> = None;
    for &l in &all_lines {
        if let Some(p) = prev {
            let gap = line_top[&l] - (line_top[&p] + line_height[&p]);
            if gap > line_height[&p].max(line_height[&l]).max(0.6) * cal.vertical_gap_ratio {
                band += 1;
            }
        }
        band_of.insert(l, band);
        prev = Some(l);
    }
    let Some(&target_band) = band_of.get(&target_line) else {
        return (0, flat.len() - 1);
    };

    let (column_of, _) = line_columns_with(flat, cal.column_gap_cqw);
    let Some(target_col) = column_of.get(&target_line).copied() else {
        return (0, flat.len() - 1);
    };
    let mut col_lines: Vec<usize> = line_top
        .keys()
        .copied()
        .filter(|l| column_of.get(l) == Some(&target_col))
        .collect();
    col_lines.sort_by(|a, b| {
        line_top[a]
            .partial_cmp(&line_top[b])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let Some(pos) = col_lines.iter().position(|&l| l == target_line) else {
        return (0, flat.len() - 1);
    };

    let mut lo = pos;
    while lo > 0
        && band_of.get(&col_lines[lo - 1]) == Some(&target_band)
        && line_is_formula(flat, col_lines[lo - 1])
    {
        lo -= 1;
    }
    let mut hi = pos;
    while hi + 1 < col_lines.len()
        && band_of.get(&col_lines[hi + 1]) == Some(&target_band)
        && line_is_formula(flat, col_lines[hi + 1])
    {
        hi += 1;
    }

    let run_lines: HashSet<usize> = col_lines[lo..=hi].iter().copied().collect();
    let start = flat
        .iter()
        .position(|w| run_lines.contains(&w.line) && !w.text.trim().is_empty())
        .unwrap_or(0);
    let end = flat
        .iter()
        .rposition(|w| run_lines.contains(&w.line) && !w.text.trim().is_empty())
        .unwrap_or(flat.len() - 1);
    (start, end)
}

/// 按阅读顺序选择段落:
/// 可向前续接到上一正文列/上一页(右栏 → 左栏),也可向后续接到
/// 下一正文列/下一页(左栏 → 右栏);首行未缩进才续接,最多各跨 1 页。
fn paragraph_walk(
    flat: &[FlatWord],
    idx: usize,
    prev_flat: Option<&[FlatWord]>,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
) -> Vec<SelectionStep> {
    paragraph_walk_with(
        flat,
        idx,
        prev_flat,
        next_flat,
        start_page,
        crate::calibration::current(),
    )
}

fn paragraph_walk_with(
    flat: &[FlatWord],
    idx: usize,
    prev_flat: Option<&[FlatWord]>,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    if flat.is_empty() {
        return Vec::new();
    }
    let anchor = snap_to_word(flat, idx).min(flat.len() - 1);
    if flat[anchor].formula {
        return paragraph_walk_formula_with(flat, anchor, next_flat, start_page, cal);
    }
    let mut steps = paragraph_backward_with(flat, anchor, prev_flat, start_page, cal);
    steps.extend(paragraph_forward_with(
        flat, anchor, next_flat, start_page, cal,
    ));
    steps
}

/// 公式块:同一行带内的整组公式;块末到栏底且本页没有更靠下的公式行时,
/// 按同列续接到下一页(最多 1 页)。
fn paragraph_walk_formula_with(
    flat: &[FlatWord],
    anchor: usize,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    let mut steps = Vec::new();
    let layout = PageLayout::new_with(flat, cal);
    let Some(col) = layout.col_of(flat, anchor) else {
        return steps;
    };
    let rep = layout.rep(col);
    let (lo, hi) = formula_block_range_with(flat, anchor, cal);
    steps.push(SelectionStep {
        page: start_page,
        lo,
        hi,
        column_left: Some(rep),
    });

    let end_top = flat[hi].top_cqw;
    let has_more_formula = (0..flat.len()).any(|i| {
        let w = &flat[i];
        !w.text.trim().is_empty()
            && w.formula
            && layout.col_of(flat, i) == Some(col)
            && w.top_cqw > end_top + 0.5
    });
    if has_more_formula {
        return steps;
    }
    if let Some(next) = next_flat {
        let next_layout = PageLayout::new_with(next, cal);
        if let Some(ncol) = next_layout.reps.iter().position(|r| (r - rep).abs() < 1.0) {
            if let Some(&first) = next_layout.word_indices(next, ncol).first() {
                if next[first].formula && !column_first_line_indented(next, &next_layout, ncol) {
                    steps.extend(paragraph_walk_with(
                        next,
                        first,
                        None,
                        None,
                        start_page + 1,
                        cal,
                    ));
                }
            }
        }
    }
    steps
}

/// 从锚点列向前(右栏 → 左栏 → 上一页)补全段落前半部分。
fn paragraph_backward_with(
    flat: &[FlatWord],
    anchor: usize,
    prev_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    let layout = PageLayout::new_with(flat, cal);
    let Some(col) = layout.col_of(flat, anchor) else {
        return Vec::new();
    };
    let (lo, _) = paragraph_range_with(flat, anchor, cal);
    let Some(&first_of_col) = layout.word_indices(flat, col).first() else {
        return Vec::new();
    };
    // 段落必须从该列第一行开始,且首行未缩进(否则是另起一段)
    if flat[lo].line != flat[first_of_col].line {
        return Vec::new();
    }
    if column_first_line_indented(flat, &layout, col) {
        return Vec::new();
    }

    // 同页前一正文列
    if let Some(prev_col) = layout.previous_text_column(flat, col) {
        let prev_words = layout.word_indices(flat, prev_col);
        let Some(&prev_last) = prev_words.last() else {
            return Vec::new();
        };
        let (plo, phi) = paragraph_range_with(flat, prev_last, cal);
        if flat[phi].line != flat[prev_last].line {
            return Vec::new(); // 前一列段落未到栏底
        }
        let mut steps = paragraph_backward_with(flat, prev_last, prev_flat, start_page, cal);
        steps.push(SelectionStep {
            page: start_page,
            lo: plo,
            hi: phi,
            column_left: Some(layout.rep(prev_col)),
        });
        return steps;
    }

    // 上一页最后正文列(最多 1 页)
    if let Some(prev) = prev_flat {
        let prev_layout = PageLayout::new_with(prev, cal);
        if let Some(&prev_col) = prev_layout.text_columns_in_content_order(prev).last() {
            if let Some(&prev_last) = prev_layout.word_indices(prev, prev_col).last() {
                let (plo, phi) = paragraph_range_with(prev, prev_last, cal);
                if prev[phi].line == prev[prev_last].line
                    && !column_first_line_indented(prev, &prev_layout, prev_col)
                {
                    return vec![SelectionStep {
                        page: start_page.saturating_sub(1),
                        lo: plo,
                        hi: phi,
                        column_left: Some(prev_layout.rep(prev_col)),
                    }];
                }
            }
        }
    }
    Vec::new()
}

/// 从锚点列向后(左栏 → 右栏 → 下一页)补全段落后半部分。
fn paragraph_forward_with(
    flat: &[FlatWord],
    idx: usize,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::calibration::DocCalibration,
) -> Vec<SelectionStep> {
    let mut steps = Vec::new();
    if flat.is_empty() {
        return steps;
    }
    let mut page = start_page;
    let mut current_flat = flat;
    let mut anchor = snap_to_word(flat, idx).min(flat.len() - 1);
    let mut crossed_page = false;
    loop {
        let layout = PageLayout::new_with(current_flat, cal);
        let Some(col) = layout.col_of(current_flat, anchor) else {
            break;
        };
        let rep = layout.rep(col);
        let (lo, hi) = paragraph_range_with(current_flat, anchor, cal);
        steps.push(SelectionStep {
            page,
            lo,
            hi,
            column_left: Some(rep),
        });

        let words = layout.word_indices(current_flat, col);
        let last_word = words.last().copied().unwrap_or(hi);
        if current_flat[hi].line != current_flat[last_word].line {
            break; // 段落在栏中间结束
        }

        if let Some(next_col) = layout.next_text_column(current_flat, col) {
            if let Some(&first) = layout.word_indices(current_flat, next_col).first() {
                if !column_first_line_indented(current_flat, &layout, next_col) {
                    anchor = first;
                    continue;
                }
                break;
            }
        }

        if !crossed_page {
            if let Some(next) = next_flat {
                let next_layout = PageLayout::new_with(next, cal);
                if let Some(ncol) =
                    (0..next_layout.reps.len()).find(|&c| next_layout.is_text_column(c))
                {
                    if let Some(&first) = next_layout.word_indices(next, ncol).first() {
                        if !column_first_line_indented(next, &next_layout, ncol) {
                            page += 1;
                            current_flat = next;
                            anchor = first;
                            crossed_page = true;
                            continue;
                        }
                    }
                }
            }
        }
        break;
    }
    steps
}

// ── 自绘选区交互(页 + 层) ──
//
// 单击无动作;拖动选词;双击选词;三击选句;Cmd+双击选段;Cmd+C 复制。
// 拖动禁止跨层/跨页;句子/段落可向后跨 1 页(同层同列)。

fn start_drag(mut session: Signal<Option<ReaderSession>>, page: u32, layer: Layer, idx: usize) {
    if let Some(inner) = session.write().as_mut() {
        inner.drag_anchor = Some((page, layer, idx));
        inner.dragging = true;
        inner.selection = None;
        inner.action_bar = None;
        inner.translation = None;
    }
}

fn extend_drag(mut session: Signal<Option<ReaderSession>>, page: u32, layer: Layer, idx: usize) {
    let (formula, formula_score) = {
        let guard = session.read();
        guard
            .as_ref()
            .and_then(|inner| {
                inner.drag_anchor.and_then(|(apage, alayer, aidx)| {
                    inner
                        .cache
                        .get(&apage)
                        .map(|r| layer_flat(r, alayer))
                        .and_then(|f| f.get(aidx))
                        .map(|w| (w.formula, w.formula_score))
                })
            })
            .unwrap_or((false, 0.0))
    };
    if let Some(inner) = session.write().as_mut() {
        if inner.dragging {
            if let Some((apage, alayer, aidx)) = inner.drag_anchor {
                if apage == page && alayer == layer && idx != aidx {
                    inner.selection = Some(Selection {
                        layer,
                        formula,
                        formula_score,
                        steps: vec![SelectionStep {
                            page,
                            lo: aidx.min(idx),
                            hi: aidx.max(idx),
                            column_left: None,
                        }],
                    });
                }
            }
        }
    }
}

fn end_drag(mut session: Signal<Option<ReaderSession>>) {
    if let Some(inner) = session.write().as_mut() {
        inner.dragging = false;
        inner.drag_anchor = None;
    }
}

fn cancel_drag(mut session: Signal<Option<ReaderSession>>) {
    if let Some(inner) = session.write().as_mut() {
        inner.dragging = false;
        inner.drag_anchor = None;
    }
}

/// 右键打开选区操作栏;公式选区且置信度足够时显示「精确复制公式」。
fn open_action_bar(mut session: Signal<Option<ReaderSession>>, x: f64, y: f64) {
    if let Some(inner) = session.write().as_mut() {
        let Some(sel) = inner.selection.as_ref() else {
            inner.action_bar = None;
            inner.translation = None;
            return;
        };
        if sel.steps.is_empty() {
            inner.action_bar = None;
            inner.translation = None;
            return;
        }
        let show_formula = sel.formula && sel.formula_score >= FORMULA_SCORE_THRESHOLD;
        let translation_enabled = crate::translate::translation_config().is_some()
            && selection_has_plain_text(sel, |p| {
                inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
            });
        inner.action_bar_gen += 1;
        inner.action_bar = Some(ActionBarState {
            x,
            y,
            status: ActionBarStatus::Idle,
            generation: inner.action_bar_gen,
            show_formula,
            translation_enabled,
        });
    }
}

fn close_action_bar(mut session: Signal<Option<ReaderSession>>) {
    if let Some(inner) = session.write().as_mut() {
        inner.action_bar = None;
        inner.translation = None;
    }
}

/// 操作栏「复制」:普通文本复制,不走 OCR。
fn action_bar_plain_copy(
    session: Signal<Option<ReaderSession>>,
    error_signal: Signal<ErrorSignal>,
) {
    let text = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let Some(sel) = inner.selection.as_ref() else {
            return;
        };
        copy_steps(sel, |p| {
            inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
        })
    };
    if let Some(text) = text {
        let ok = copy_to_clipboard(&text);
        copy_feedback(error_signal, ok);
        close_action_bar(session);
    }
}

/// 操作栏「翻译」:快照选区文本/公式/配置,异步请求后写入翻译卡片。
fn action_bar_translate(mut session: Signal<Option<ReaderSession>>) {
    let request = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let Some(sel) = inner.selection.as_ref() else {
            return;
        };
        let Some(bar) = inner.action_bar.as_ref() else {
            return;
        };
        if !bar.translation_enabled {
            return;
        }
        let Some((input, formulas)) = selection_translation_input(sel, |p| {
            inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
        }) else {
            return;
        };
        if input.trim().is_empty() {
            return;
        }
        let Some(config) = crate::translate::translation_config() else {
            return;
        };
        (input, formulas, config, bar.x, bar.y)
    };
    let generation = {
        let mut inner = session.write();
        let Some(inner) = inner.as_mut() else {
            return;
        };
        inner.translation_gen += 1;
        let translation_generation = inner.translation_gen;
        // 开始翻译后关闭操作栏，只保留翻译卡片。
        inner.action_bar = None;
        inner.translation = Some(TranslationCardState {
            x: request.3,
            y: request.4 + ACTION_BAR_CARD_OFFSET_Y,
            status: ActionBarStatus::Loading,
            generation: translation_generation,
            text: String::new(),
        });
        translation_generation
    };
    spawn(async move {
        use futures::StreamExt;
        let mut stream = match crate::translate::translation_stream(&request.2, &request.0).await {
            Ok(s) => s,
            Err(e) => {
                let mut inner = session.write();
                if let Some(tc) = inner.as_mut().and_then(|s| s.translation.as_mut()) {
                    if tc.generation == generation {
                        tc.status = ActionBarStatus::Error;
                        tc.text = e;
                    }
                }
                return;
            }
        };
        let mut buf = String::new();
        let mut stream_error: Option<String> = None;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(llm::Chunk::Text(t)) => {
                    buf.push_str(&t);
                    // 流式显示:实时更新卡片(原始文本,完成后统一清洗/回填)
                    let mut inner = session.write();
                    if let Some(tc) = inner.as_mut().and_then(|s| s.translation.as_mut()) {
                        if tc.generation == generation {
                            tc.text = crate::translate::stream_visible(&buf);
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    stream_error = Some(format!("{e}"));
                    break;
                }
            }
        }
        let (status, text) = match stream_error {
            Some(e) => (ActionBarStatus::Error, e),
            None => {
                let cleaned = crate::translate::clean_translation(&buf);
                if cleaned.is_empty() {
                    (ActionBarStatus::Error, "翻译结果为空".to_string())
                } else {
                    let text = crate::translate::reinsert_formulas(&cleaned, &request.1);
                    (ActionBarStatus::Success, text)
                }
            }
        };
        let mut inner = session.write();
        let Some(inner) = inner.as_mut() else {
            return;
        };
        let Some(tc) = inner.translation.as_mut() else {
            return;
        };
        if tc.generation != generation {
            return;
        }
        tc.status = status;
        tc.text = text;
    });
}

/// 操作栏「精确复制公式」:只有公式选区可用;命中缓存直接复制,否则投递 OCR worker。
fn action_bar_formula_copy(
    mut session: Signal<Option<ReaderSession>>,
    error_signal: Signal<ErrorSignal>,
) {
    enum Outcome {
        Cached(String),
        Request(CopyRequest),
    }
    let action = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let Some(sel) = inner.selection.as_ref() else {
            return;
        };
        if !sel.formula || sel.formula_score < FORMULA_SCORE_THRESHOLD {
            return;
        }
        if sel.steps.len() != 1 {
            return;
        }
        let step = &sel.steps[0];
        let page = step.page;
        let Some(r) = inner.cache.get(&page) else {
            return;
        };
        let flat = layer_flat(r, sel.layer);
        let bbox = selection_bbox_px(flat, step, r.w_pt);
        let key = formula_copy_key(&inner.book_id, page, bbox);
        if let Some(latex) = inner.ocr_cache.get(&key) {
            Outcome::Cached(latex.to_string())
        } else {
            let same_pending = inner
                .pending_copy
                .as_ref()
                .map(|p| p.key == key)
                .unwrap_or(false);
            if same_pending {
                return;
            }
            let text = copy_steps(sel, |p| {
                inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
            });
            Outcome::Request(CopyRequest {
                page: (page as usize).saturating_sub(1),
                key,
                bbox,
                doc: inner.doc.clone(),
                fallback: crate::formula_ocr::latex_escape_unicode(&text.unwrap_or_default()),
            })
        }
    };
    match action {
        Outcome::Cached(latex) => {
            let _ = copy_to_clipboard(&latex);
            let generation = {
                let mut inner = session.write();
                let Some(inner) = inner.as_mut() else {
                    return;
                };
                let Some(bar) = inner.action_bar.as_mut() else {
                    return;
                };
                bar.status = ActionBarStatus::Success;
                bar.generation
            };
            schedule_action_bar_reset(session, generation);
        }
        Outcome::Request(req) => {
            let should_spawn = {
                let mut inner = session.write();
                let Some(inner) = inner.as_mut() else {
                    return;
                };
                inner.pending_copy = Some(req);
                if let Some(bar) = inner.action_bar.as_mut() {
                    bar.status = ActionBarStatus::Loading;
                }
                let spawn = !inner.copy_busy;
                if spawn {
                    inner.copy_busy = true;
                }
                spawn
            };
            if should_spawn {
                spawn_copy_worker(session, error_signal);
            }
        }
    }
}

/// 成功文案停留 2.5s 后复位;代数不匹配(栏已关闭/重开)时丢弃。
fn schedule_action_bar_reset(mut session: Signal<Option<ReaderSession>>, generation: u64) {
    spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(ACTION_BAR_SUCCESS_MS)).await;
        if let Some(inner) = session.write().as_mut() {
            let current = inner.action_bar.as_ref().map(|b| (b.generation, b.status));
            if current == Some((generation, ActionBarStatus::Success)) {
                if let Some(bar) = inner.action_bar.as_mut() {
                    bar.status = ActionBarStatus::Idle;
                }
            }
        }
    });
}

fn select_word(mut session: Signal<Option<ReaderSession>>, page: u32, layer: Layer, idx: usize) {
    let (idx, formula, formula_score) = {
        let guard = session.read();
        guard
            .as_ref()
            .and_then(|inner| {
                inner
                    .cache
                    .get(&page)
                    .map(|r| layer_flat(r, layer))
                    .map(|flat| {
                        let i = snap_to_word(flat, idx);
                        (i, flat[i].formula, flat[i].formula_score)
                    })
            })
            .unwrap_or((idx, false, 0.0))
    };
    if let Some(inner) = session.write().as_mut() {
        inner.selection = Some(Selection {
            layer,
            formula,
            formula_score,
            steps: vec![SelectionStep {
                page,
                lo: idx,
                hi: idx,
                column_left: None,
            }],
        });
        inner.dragging = false;
        inner.drag_anchor = None;
        inner.action_bar = None;
        inner.translation = None;
    }
}

fn layer_flat<'a>(page: &'a RenderedPage, layer: Layer) -> &'a [FlatWord] {
    match layer {
        Layer::Body => &page.body,
        Layer::Small => &page.small,
    }
}

/// 三击:按阅读顺序选当前句(同页跨栏、最多跨 1 页)。
fn select_sentence(
    mut session: Signal<Option<ReaderSession>>,
    page: u32,
    layer: Layer,
    idx: usize,
) {
    let selection = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let Some(r) = inner.cache.get(&page) else {
            return;
        };
        let flat = layer_flat(r, layer);
        let anchor = snap_to_word(flat, idx).min(flat.len() - 1);
        let prev_flat = page
            .checked_sub(1)
            .and_then(|p| inner.cache.get(&p))
            .map(|r| layer_flat(r, layer));
        let next_flat = inner.cache.get(&(page + 1)).map(|r| layer_flat(r, layer));
        Selection {
            layer,
            formula: flat[anchor].formula,
            formula_score: flat[anchor].formula_score,
            steps: sentence_walk(flat, anchor, prev_flat, next_flat, page),
        }
    };
    if let Some(inner) = session.write().as_mut() {
        inner.selection = Some(selection);
        inner.dragging = false;
        inner.drag_anchor = None;
        inner.action_bar = None;
        inner.translation = None;
    }
}

/// Cmd+双击:选当前段,下一页同列首行未缩进则向后跨 1 页。
fn select_paragraph(
    mut session: Signal<Option<ReaderSession>>,
    page: u32,
    layer: Layer,
    idx: usize,
) {
    let selection = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let Some(r) = inner.cache.get(&page) else {
            return;
        };
        let flat = layer_flat(r, layer);
        let anchor = snap_to_word(flat, idx).min(flat.len() - 1);
        let prev_flat = page
            .checked_sub(1)
            .and_then(|p| inner.cache.get(&p))
            .map(|r| layer_flat(r, layer));
        let next_flat = inner.cache.get(&(page + 1)).map(|r| layer_flat(r, layer));
        Selection {
            layer,
            formula: flat[anchor].formula,
            formula_score: flat[anchor].formula_score,
            steps: paragraph_walk(flat, anchor, prev_flat, next_flat, page),
        }
    };
    if let Some(inner) = session.write().as_mut() {
        inner.selection = Some(selection);
        inner.dragging = false;
        inner.drag_anchor = None;
        inner.action_bar = None;
        inner.translation = None;
    }
}

/// 点击页面空白:清除选区、重置点击计数并聚焦滚动容器。
fn page_mousedown(
    mut session: Signal<Option<ReaderSession>>,
    desktop: dioxus::desktop::DesktopContext,
    mut click_state: Signal<(u32, u32, Layer, usize, u64, std::time::Instant)>,
    page: u32,
    evt: MouseEvent,
) {
    if evt.trigger_button() == Some(dioxus::html::input_data::MouseButton::Secondary) {
        return; // 右键交给操作栏,不清选区
    }
    if let Some(inner) = session.write().as_mut() {
        inner.selection = None;
        inner.drag_anchor = None;
        inner.dragging = false;
        inner.action_bar = None;
        inner.translation = None;
    }
    let (_, _, _, _, generation, _) = *click_state.read();
    click_state.set((
        0,
        page,
        Layer::Body,
        usize::MAX,
        generation + 1,
        std::time::Instant::now(),
    ));
    let _ = desktop
        .webview
        .evaluate_script("var el=document.querySelector('.reader-scroll');if(el)el.focus();");
}

/// 文字层上的双击兜底(两次点击落在不同词/跨层时,用最近点过的词)。
fn page_doubleclick(
    session: Signal<Option<ReaderSession>>,
    click_state: Signal<(u32, u32, Layer, usize, u64, std::time::Instant)>,
    evt: MouseEvent,
    page: u32,
) {
    let (_, click_page, layer, word, _, _) = *click_state.read();
    if click_page == page && word != usize::MAX {
        if evt.modifiers().contains(Modifiers::META) {
            select_paragraph(session, page, layer, word);
        } else {
            select_word(session, page, layer, word);
        }
    }
}

/// 渲染某一层的透明词层(含该层选区高亮)。
fn render_layer_overlay(
    page: u32,
    layer: Layer,
    flat: &[FlatWord],
    rects: &[SelectionRect],
    session: Signal<Option<ReaderSession>>,
    click_state: Signal<(u32, u32, Layer, usize, u64, std::time::Instant)>,
    desktop: dioxus::desktop::DesktopContext,
) -> Element {
    rsx! {
        div {
            class: "reader-text-overlay",
            for fw in flat {
                {
                    let idx = fw.gesture;
                    let p = page;
                    let layer = layer;
                    let d = desktop.clone();
                    let cs = click_state;
                    rsx! {
                        span {
                            class: if fw.text.trim().is_empty() {
                                "reader-overlay-word space-word"
                            } else {
                                "reader-overlay-word"
                            },
                            style: "left: {fw.left_cqw:.3}%; top: {fw.top_cqw:.3}cqw; width: {fw.width_cqw:.3}cqw; height: {fw.height_cqw:.3}cqw; font-size: 12px; font-size: {fw.height_cqw:.3}cqw; line-height: 12px; line-height: {fw.height_cqw:.3}cqw",
                            onmousedown: {
                                let idx = idx;
                                let p = p;
                                let layer = layer;
                                let d = d.clone();
                                move |evt: MouseEvent| {
                                    if evt.trigger_button()
                                        == Some(dioxus::html::input_data::MouseButton::Secondary)
                                    {
                                        evt.stop_propagation();
                                        return;
                                    }
                                    evt.stop_propagation();
                                    evt.prevent_default();
                                    start_drag(session, p, layer, idx);
                                    let _ = d.webview.evaluate_script(
                                        "var el=document.querySelector('.reader-scroll');if(el)el.focus();",
                                    );
                                }
                            },
                            onmouseenter: {
                                let idx = idx;
                                let p = p;
                                let layer = layer;
                                move |_| extend_drag(session, p, layer, idx)
                            },
                            onmousemove: {
                                let idx = idx;
                                let p = p;
                                let layer = layer;
                                move |_| extend_drag(session, p, layer, idx)
                            },
                            onclick: {
                                let idx = idx;
                                let p = p;
                                let layer = layer;
                                let mut cs = cs;
                                move |_| {
                                    let now = std::time::Instant::now();
                                    let (count, cpage, clayer, word, generation, last) = *cs.read();
                                    let count = if cpage == p
                                        && clayer == layer
                                        && word == idx
                                        && now.duration_since(last).as_millis() <= 350
                                    {
                                        count + 1
                                    } else {
                                        1
                                    };
                                    cs.set((
                                        count,
                                        p,
                                        layer,
                                        idx,
                                        generation + 1,
                                        now,
                                    ));
                                    if count >= 3 {
                                        cs.set((
                                            0,
                                            p,
                                            layer,
                                            usize::MAX,
                                            generation + 2,
                                            now,
                                        ));
                                        select_sentence(session, p, layer, idx);
                                    }
                                }
                            },
                            ondoubleclick: {
                                let idx = idx;
                                let p = p;
                                let layer = layer;
                                move |evt: MouseEvent| {
                                    evt.stop_propagation();
                                    if evt.modifiers().contains(Modifiers::META) {
                                        select_paragraph(session, p, layer, idx);
                                    } else {
                                        select_word(session, p, layer, idx);
                                    }
                                }
                            },
                            oncontextmenu: {
                                let idx = idx;
                                let p = p;
                                let layer = layer;
                                move |evt: MouseEvent| {
                                    evt.stop_propagation();
                                    evt.prevent_default();
                                    // 已有选区时保留选区（双指按只弹操作栏）；
                                    // 没有选区时才选中右键点到的词。
                                    let has_selection = session
                                        .read()
                                        .as_ref()
                                        .and_then(|s| s.selection.as_ref())
                                        .map(|s| !s.steps.is_empty())
                                        .unwrap_or(false);
                                    if !has_selection {
                                        select_word(session, p, layer, idx);
                                    }
                                    let coords = evt.client_coordinates();
                                    open_action_bar(session, coords.x, coords.y);
                                }
                            },
                            "{fw.text}"
                        }
                    }
                }
            }
            for rect in rects {
                div {
                    class: "reader-selection",
                    style: "left: {rect.left_cqw:.3}%; top: {rect.top_cqw:.3}cqw; width: {rect.width_cqw:.3}cqw; height: {rect.height_cqw:.3}cqw",
                }
            }
        }
    }
}

/// 滚动接近底部时渲染下一批页面。
fn maybe_load_more(
    mut session: Signal<Option<ReaderSession>>,
    error_signal: Signal<ErrorSignal>,
    scroll_state: Signal<(f64, f64, f64)>,
) {
    let should = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        if inner.loading_more || inner.rendered_until >= inner.page_count {
            return;
        }
        true
    };
    if !should {
        return;
    }
    if let Some(inner) = session.write().as_mut() {
        inner.loading_more = true;
    }

    spawn(async move {
        loop {
            let (from, to, doc) = {
                let guard = session.read();
                let Some(inner) = guard.as_ref() else {
                    break;
                };
                let from = inner.rendered_until + 1;
                if from > inner.page_count {
                    break;
                }
                let to = (from + BATCH_SIZE - 1).min(inner.page_count);
                (from, to, inner.doc.clone())
            };
            let mut last_rendered = from - 1;
            for p in from..=to {
                let doc = doc.clone();
                let result =
                    tokio::task::spawn_blocking(move || render_page_with_overlay(&doc, p - 1))
                        .await;
                match result {
                    Ok(Ok(rendered)) => {
                        if let Some(inner) = session.write_unchecked().as_mut() {
                            inner.cache.insert(p, rendered);
                            inner.rendered_until = p;
                            if inner.cache.len() > MAX_CACHE_PAGES {
                                let keep_from = inner
                                    .rendered_until
                                    .saturating_sub(MAX_CACHE_PAGES as u32 - 5);
                                inner.cache.retain(|&k, _| k >= keep_from);
                            }
                        }
                        last_rendered = p;
                    }
                    Ok(Err(e)) => {
                        let mut err = error_signal;
                        err.write().push(ErrorInfo::new(
                            "reader-render-failed",
                            "渲染页面失败",
                            e,
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                        break;
                    }
                    Err(e) => {
                        let mut err = error_signal;
                        err.write().push(ErrorInfo::new(
                            "reader-render-failed",
                            "渲染页面失败",
                            format!("{e}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                        break;
                    }
                }
            }
            let (top, ch, sh) = *scroll_state.read();
            if top + ch < sh - NEAR_BOTTOM_PX || last_rendered >= to {
                break;
            }
        }
        if let Some(inner) = session.write_unchecked().as_mut() {
            inner.loading_more = false;
        }
    });
}

#[component]
pub fn ReaderPanel(
    book_id: String,
    error_signal: Signal<ErrorSignal>,
    on_back: Callback<()>,
) -> Element {
    let session = use_signal(|| Option::<ReaderSession>::None);
    let zoom = use_signal(|| 100u32);
    let desktop = use_window();
    // (连续点击次数, 页, 层, 词索引, 代数, 最后点击时间)
    let click_state = use_signal(|| {
        (
            0u32,
            0u32,
            Layer::Body,
            usize::MAX,
            0u64,
            std::time::Instant::now(),
        )
    });
    // (scroll_top, client_height, scroll_height)
    let scroll_state = use_signal(|| (0.0f64, 0.0f64, 0.0f64));
    // 翻译卡片拖动中：(指针起点 x, y, 卡片原点 x, y)
    let drag_card = use_signal(|| Option::<(f64, f64, f64, f64)>::None);

    let mut opened = use_signal(|| false);
    use_effect(move || {
        if *opened.read() {
            return;
        }
        opened.set(true);
        let book_id = book_id.clone();
        let mut session = session;
        let mut err = error_signal;
        spawn(async move {
            let parse_id = book_id.clone();
            let opened = tokio::task::spawn_blocking(move || {
                let book = crate::db::with_db(|conn| crate::books::get(conn, &book_id))
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| format!("book not found: {book_id}"))?;
                let book_dir = PathBuf::from(&book.path);
                let pdf_path = crate::layout::book_pdf_path(&book_dir);
                let doc = pdfium::open(&pdf_path).map_err(|e| format!("{e:#}"))?;
                let page_count = doc.page_count();
                let _ = crate::calibration::ensure_for_book(&book_id, &book_dir, &doc);
                let first = render_page_with_overlay(&doc, 0)?;
                Ok::<_, String>((book.name, Arc::new(doc), page_count, first, book_dir))
            })
            .await;
            match opened {
                Ok(Ok((book_name, doc, page_count, first, book_dir))) => {
                    let mut cache = HashMap::new();
                    cache.insert(1, first);
                    session.set(Some(ReaderSession {
                        doc: doc.clone(),
                        page_count,
                        cache,
                        rendered_until: 1,
                        loading_more: false,
                        book_id: parse_id.clone(),
                        book_name,
                        selection: None,
                        drag_anchor: None,
                        dragging: false,
                        ocr_cache: SingleSlotCache::new(),
                        pending_copy: None,
                        copy_busy: false,
                        action_bar: None,
                        action_bar_gen: 0,
                        translation: None,
                        translation_gen: 0,
                    }));

                    for p in 2..=3u32.min(page_count) {
                        let doc = doc.clone();
                        let result = tokio::task::spawn_blocking(move || {
                            render_page_with_overlay(&doc, p - 1)
                        })
                        .await;
                        if let Ok(Ok(rendered)) = result {
                            if let Some(inner) = session.write_unchecked().as_mut() {
                                inner.cache.insert(p, rendered);
                                inner.rendered_until = p;
                            }
                        }
                    }

                    if crate::pdf::read_parse_marker(&book_dir).is_none() {
                        spawn(async move {
                            let result =
                                tokio::task::spawn_blocking(move || parse_book(&parse_id)).await;
                            match result {
                                Ok(Ok(_)) => {}
                                Ok(Err(e)) => {
                                    err.write().push(ErrorInfo::new(
                                        "reader-parse-failed",
                                        "提取文本失败",
                                        format!("{e:#}"),
                                        ErrorSeverity::Warning,
                                        ErrorSource::General,
                                    ));
                                }
                                Err(e) => {
                                    err.write().push(ErrorInfo::new(
                                        "reader-parse-failed",
                                        "提取文本失败",
                                        format!("{e}"),
                                        ErrorSeverity::Warning,
                                        ErrorSource::General,
                                    ));
                                }
                            }
                        });
                    }
                }
                Ok(Err(e)) => {
                    err.write().push(ErrorInfo::new(
                        "reader-open-failed",
                        "打开 PDF 失败",
                        e,
                        ErrorSeverity::Warning,
                        ErrorSource::General,
                    ));
                }
                Err(e) => {
                    err.write().push(ErrorInfo::new(
                        "reader-open-failed",
                        "打开 PDF 失败",
                        format!("{e}"),
                        ErrorSeverity::Warning,
                        ErrorSource::General,
                    ));
                }
            }
        });
    });

    let on_scroll = move |evt: ScrollEvent| {
        let top = evt.scroll_top();
        let ch = evt.client_height() as f64;
        let sh = evt.scroll_height() as f64;
        let mut ss = scroll_state;
        ss.set((top, ch, sh));
        // 有翻译卡片时，滚动不关闭操作栏和翻译块（选区与浮层保持可见）。
        let has_translation = session
            .read()
            .as_ref()
            .and_then(|s| s.translation.as_ref())
            .is_some();
        if !has_translation {
            close_action_bar(session);
        }
        if top + ch >= sh - NEAR_BOTTOM_PX {
            maybe_load_more(session, error_signal, scroll_state);
        }
    };

    let on_root_mouseup = move |_| {
        end_drag(session);
        let mut drag_card = drag_card;
        drag_card.set(None);
    };
    let on_root_mousemove = {
        let mut session = session;
        let drag_card = drag_card;
        move |evt: MouseEvent| {
            let Some((start_x, start_y, origin_x, origin_y)) = *drag_card.read() else {
                return;
            };
            let coords = evt.client_coordinates();
            let cx = coords.x;
            let cy = coords.y;
            let nx = (origin_x + (cx - start_x)).max(0.0);
            let ny = (origin_y + (cy - start_y)).max(0.0);
            if let Some(inner) = session.write().as_mut() {
                if let Some(tc) = inner.translation.as_mut() {
                    tc.x = nx;
                    tc.y = ny;
                }
            }
        }
    };
    let on_scroll_mouseleave = move |_| cancel_drag(session);

    let on_keydown = {
        let session = session;
        let err = error_signal;
        move |evt: KeyboardEvent| {
            if evt.key().to_string() == "Escape" {
                close_action_bar(session);
                return;
            }
            if !evt.modifiers().contains(Modifiers::META)
                || !evt.key().to_string().eq_ignore_ascii_case("c")
            {
                return;
            }
            // ⌘C 永远是普通文本复制;公式 OCR 只走操作栏「精确复制公式」。
            let text = {
                let guard = session.read();
                let Some(inner) = guard.as_ref() else {
                    return;
                };
                let Some(sel) = inner.selection.as_ref() else {
                    return;
                };
                copy_steps(sel, |p| {
                    inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
                })
            };
            if let Some(text) = text {
                let ok = copy_to_clipboard(&text);
                copy_feedback(err, ok);
                close_action_bar(session);
            }
        }
    };

    let on_zoom_out = move |_| {
        let mut z = zoom;
        let next = (z() as i64 - ZOOM_STEP as i64).clamp(ZOOM_MIN as i64, ZOOM_MAX as i64) as u32;
        z.set(next);
    };
    let on_zoom_in = move |_| {
        let mut z = zoom;
        let next = (z() as i64 + ZOOM_STEP as i64).clamp(ZOOM_MIN as i64, ZOOM_MAX as i64) as u32;
        z.set(next);
    };

    let (
        book_name,
        page_count,
        rendered_until,
        zoom_now,
        selection,
        copy_busy,
        action_bar,
        translation,
    ) = session
        .read()
        .as_ref()
        .map(|s| {
            (
                s.book_name.clone(),
                s.page_count,
                s.rendered_until,
                zoom(),
                s.selection.clone(),
                s.copy_busy,
                s.action_bar,
                s.translation.clone(),
            )
        })
        .unwrap_or_else(|| (String::new(), 0u32, 0u32, 100u32, None, false, None, None));

    let action_bar_view = action_bar.map(|bar| {
        let status_class = match bar.status {
            ActionBarStatus::Idle => "",
            ActionBarStatus::Loading => "is-loading",
            ActionBarStatus::Error => "is-error",
            ActionBarStatus::Success => "is-success",
        };
        let label = match bar.status {
            ActionBarStatus::Idle => "精确复制公式",
            ActionBarStatus::Loading => "识别中…",
            ActionBarStatus::Error => "识别失败，重试",
            ActionBarStatus::Success => "已复制 LaTeX",
        };
        let translation_loading = translation
            .as_ref()
            .map(|t| t.status == ActionBarStatus::Loading)
            .unwrap_or(false);
        (
            bar,
            status_class,
            label,
            bar.show_formula,
            bar.translation_enabled,
            translation_loading,
        )
    });

    let translation_view = translation.as_ref().map(|tc| {
        let status_class = match tc.status {
            ActionBarStatus::Idle => "",
            ActionBarStatus::Loading => "is-loading",
            ActionBarStatus::Error => "is-error",
            ActionBarStatus::Success => "is-success",
        };
        (tc.x, tc.y, tc.status, tc.text.clone(), status_class)
    });

    rsx! {
        div {
            class: "reader-root",
            onmouseup: on_root_mouseup,
            onmousemove: on_root_mousemove,
            div {
                class: "reader-topbar",
                onmousedown: move |_| close_action_bar(session),
                button {
                    class: "btn btn-cancel",
                    onclick: move |_| on_back.call(()),
                    "← 书库"
                }
                span { class: "reader-title", "{book_name}" }
                div {
                    class: "reader-toolbar",
                    span { class: "reader-page-total", "已加载 {rendered_until}/{page_count}" }
                    if copy_busy {
                        span { class: "reader-copy-status", "识别公式中…" }
                    }
                    button {
                        class: "btn btn-cancel",
                        onclick: on_zoom_out,
                        "−"
                    }
                    span { class: "reader-zoom", "{zoom_now}%" }
                    button {
                        class: "btn btn-cancel",
                        onclick: on_zoom_in,
                        "+"
                    }
                }
            }
            if session.read().is_none() {
                div {
                    class: "reader-loading",
                    "打开 PDF 中…"
                }
            } else {
                div {
                    class: "reader-scroll",
                    tabindex: "-1",
                    onscroll: on_scroll,
                    onmouseleave: on_scroll_mouseleave,
                    onkeydown: on_keydown,
                    oncontextmenu: move |evt| {
                        evt.prevent_default();
                        let coords = evt.client_coordinates();
                        open_action_bar(session, coords.x, coords.y);
                    },
                    for page in 1..=rendered_until {
                        {
                            let page = page;
                            let session = session;
                            let click_state = click_state;
                            let desktop = desktop.clone();
                            let selection = selection.as_ref();
                            rsx! {
                                {
                                    let guard = session.read();
                                    match guard.as_ref().and_then(|inner| inner.cache.get(&page)) {
                                        Some(r) => {
                                            let rects_for =
                                                |flat: &[FlatWord], layer: Layer| -> Vec<SelectionRect> {
                                                    match selection {
                                                        Some(sel) if sel.layer == layer => sel
                                                            .steps
                                                            .iter()
                                                            .filter(|s| s.page == page)
                                                            .flat_map(|s| {
                                                                selection_rects_filtered(
                                                                    flat,
                                                                    s.lo.min(flat.len() - 1),
                                                                    s.hi.min(flat.len() - 1),
                                                                    s.column_left,
                                                                )
                                                            })
                                                            .collect(),
                                                        _ => Vec::new(),
                                                    }
                                                };
                                            let body_rects = rects_for(&r.body, Layer::Body);
                                            let small_rects = rects_for(&r.small, Layer::Small);
                                            rsx! {
                                                div {
                                                    class: "reader-page-view",
                                                    style: "width: {zoom_now}%; aspect-ratio: {r.w_pt} / {r.h_pt}",
                                                    onmousedown: {
                                                        let p = page;
                                                        let d = desktop.clone();
                                                        let cs = click_state;
                                                        move |evt: MouseEvent| page_mousedown(session, d.clone(), cs, p, evt)
                                                    },
                                                    ondoubleclick: {
                                                        let p = page;
                                                        move |evt: MouseEvent| page_doubleclick(session, click_state, evt, p)
                                                    },
                                                    img {
                                                        class: "reader-page-img",
                                                        src: "{r.src}",
                                                        draggable: "false",
                                                    }
                                                    { render_layer_overlay(page, Layer::Body, &r.body, &body_rects, session, click_state, desktop.clone()) }
                                                    { render_layer_overlay(page, Layer::Small, &r.small, &small_rects, session, click_state, desktop.clone()) }
                                                }
                                            }
                                        }
                                        None => rsx! {},
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if let Some((bar, status_class, label, show_formula, translation_enabled, translation_loading)) = action_bar_view {
                div {
                    class: "reader-actionbar is-open",
                    role: "toolbar",
                    "aria-label": "选区操作",
                    "aria-live": "polite",
                    style: "left: {bar.x}px; top: {bar.y}px",
                    button {
                        class: "reader-actionbar__btn",
                        onclick: move |_| action_bar_plain_copy(session, error_signal),
                        "复制"
                    }
                    if translation_enabled {
                        button {
                            class: "reader-actionbar__btn",
                            disabled: translation_loading,
                            onclick: move |_| action_bar_translate(session),
                            "翻译"
                        }
                    }
                    if show_formula {
                        span {
                            class: "reader-actionbar__divider",
                            "aria-hidden": "true",
                        }
                        button {
                            class: "reader-actionbar__btn {status_class}",
                            disabled: bar.status == ActionBarStatus::Loading,
                            "aria-busy": if bar.status == ActionBarStatus::Loading { "true" } else { "false" },
                            "aria-invalid": if bar.status == ActionBarStatus::Error { "true" } else { "false" },
                            onclick: move |_| action_bar_formula_copy(session, error_signal),
                            "{label}"
                        }
                    }
                }
            }
            if let Some((x, y, status, text, status_class)) = translation_view {
                div {
                    class: "reader-translation-card {status_class}",
                    role: "dialog",
                    "aria-label": "翻译结果",
                    style: "left: {x}px; top: {y}px",
                    div {
                        class: "reader-translation-card__head",
                        onmousedown: {
                            let x = x;
                            let y = y;
                            let mut dc = drag_card;
                            move |evt: MouseEvent| {
                                evt.stop_propagation();
                                evt.prevent_default();
                                let coords = evt.client_coordinates();
                                let cx = coords.x;
                                let cy = coords.y;
                                dc.set(Some((cx, cy, x, y)));
                            }
                        },
                        span { class: "reader-translation-card__title", "翻译" }
                        button {
                            class: "reader-translation-card__close",
                            onclick: move |_| close_action_bar(session),
                            "✕"
                        }
                    }
                    if status == ActionBarStatus::Loading {
                        div { class: "reader-translation-card__loading", "翻译中…" }
                        if !text.is_empty() {
                            div { class: "reader-translation-card__body is-streaming", "{text}" }
                        }
                    } else if status == ActionBarStatus::Success {
                        div { class: "reader-translation-card__body", "{text}" }
                    } else if status == ActionBarStatus::Error {
                        div { class: "reader-translation-card__error", "{text}" }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fw(line: usize, text: &str, left: f64, top: f64, height: f64) -> FlatWord {
        FlatWord {
            line,
            text: text.to_string(),
            left_cqw: left,
            top_cqw: top,
            width_cqw: 1.0,
            height_cqw: height,
            line_height_cqw: height,
            formula: false,
            formula_score: 0.0,
            gesture: 0,
        }
    }

    fn fwf(line: usize, text: &str, left: f64, top: f64, height: f64) -> FlatWord {
        FlatWord {
            formula: true,
            formula_score: 1.0,
            ..fw(line, text, left, top, height)
        }
    }

    fn overlay_line(words: Vec<(String, f64, f64, f64, f64)>) -> OverlayLine {
        use crate::pdf::OverlayWord;
        OverlayLine {
            top_pct: 0.0,
            height_pct: 0.0,
            height_cqw: words.first().map(|w| w.4).unwrap_or(1.0),
            words: words
                .into_iter()
                .map(|(text, left, top, width, height)| OverlayWord {
                    text,
                    left_pct: left,
                    top_cqw: top,
                    width_cqw: width,
                    height_cqw: height,
                })
                .collect(),
        }
    }

    #[test]
    fn sentence_walk_stays_in_column() {
        let flat = vec![
            fw(0, "One.", 0.0, 0.0, 1.0),
            fw(0, "Two", 2.0, 0.0, 1.0),
            fw(0, "sentences", 4.0, 0.0, 1.0),
            fw(0, "here.", 6.0, 0.0, 1.0),
        ];
        let steps = sentence_walk(&flat, 0, None, None, 0);
        assert_eq!(steps.len(), 1);
        assert_eq!((steps[0].lo, steps[0].hi), (0, 0));
        let steps = sentence_walk(&flat, 2, None, None, 0);
        assert_eq!(steps.len(), 1);
        assert_eq!((steps[0].lo, steps[0].hi), (1, 3));
    }

    #[test]
    fn sentence_walk_flows_left_to_right_column() {
        // 左栏句子未结束 → 续接到右栏;中间穿插的图注列不进入路径
        let flat = vec![
            fw(0, "We", 10.0, 0.0, 2.0),
            fw(0, " ", 13.0, 0.0, 2.0),
            fw(0, "have", 14.0, 0.0, 2.0),
            fw(1, "combined", 10.0, 5.0, 2.0),
            fw(1, " ", 20.0, 5.0, 2.0),
            fw(1, "with", 21.0, 5.0, 2.0),
            fw(2, "Figure", 60.0, 0.0, 1.0),
            fw(2, "1", 61.0, 0.0, 1.0),
            fw(3, "deep", 51.0, 0.0, 2.0),
            fw(3, " ", 56.0, 0.0, 2.0),
            fw(3, "models,", 57.0, 0.0, 2.0),
            fw(4, "model.", 51.0, 5.0, 2.0),
        ];
        let steps = sentence_walk(&flat, 3, None, None, 0);
        assert_eq!(steps.len(), 2, "左栏一步 + 右栏一步");
        assert_eq!((steps[0].lo, steps[0].hi), (0, 5));
        assert_eq!((steps[1].lo, steps[1].hi), (8, 11));
        let copied = copy_steps(
            &Selection {
                layer: Layer::Body,
                formula: false,
                formula_score: 0.0,
                steps,
            },
            |_| Some(&flat),
        )
        .unwrap();
        assert!(copied.contains("combined with\ndeep models,"), "{copied:?}");
        assert!(!copied.contains("Figure"), "{copied:?}");
    }

    #[test]
    fn sentence_walk_continues_backward_to_left_column_when_sentence_unfinished() {
        let flat = vec![
            fw(0, "L1", 10.0, 0.0, 2.0),
            fw(1, "L2", 10.0, 2.5, 2.0),
            fw(2, "R1", 51.0, 0.0, 2.0),
            fw(3, "end.", 51.0, 2.5, 2.0),
        ];
        let steps = sentence_walk(&flat, 2, None, None, 0);
        assert_eq!(steps.len(), 2, "右栏锚点应向前包含左栏未结束的句子前半");
        assert_eq!((steps[0].lo, steps[0].hi), (0, 1));
        assert_eq!((steps[1].lo, steps[1].hi), (2, 3));
    }

    #[test]
    fn sentence_walk_does_not_continue_backward_when_previous_column_ends_sentence() {
        let flat = vec![
            fw(0, "L1.", 10.0, 0.0, 2.0),
            fw(1, "R1", 51.0, 0.0, 2.0),
            fw(2, "end.", 51.0, 2.5, 2.0),
        ];
        let steps = sentence_walk(&flat, 1, None, None, 0);
        assert_eq!(steps.len(), 1, "左栏句子已结束,不向前续");
        assert_eq!((steps[0].lo, steps[0].hi), (1, 2));
    }

    #[test]
    fn sentence_walk_continues_backward_to_previous_page() {
        let page0 = vec![
            fw(0, "Furthermore,", 51.0, 0.0, 2.0),
            fw(1, "evaluated", 51.0, 2.5, 2.0),
        ];
        let page1 = vec![
            fw(0, "several", 8.8, 0.0, 2.0),
            fw(1, "benchmarks.", 8.8, 2.5, 2.0),
        ];
        let steps = sentence_walk(&page1, 0, Some(&page0), None, 1);
        assert_eq!(steps.len(), 2, "跨页向前续接上一页右栏的句子前半");
        assert_eq!(steps[0].page, 0);
        assert_eq!(steps[1].page, 1);
        assert_eq!((steps[0].lo, steps[0].hi), (0, 1));
    }

    #[test]
    fn sentence_walk_skips_table_formula_and_continues_to_body_text() {
        // 跨页后页首是表格公式列(非正文),句子应跳过它,
        // 从下一页第一个正文列继续,直到句号结束
        let page0 = vec![fw(0, "include", 10.0, 0.0, 2.0)];
        let page1 = vec![
            fwf(0, "[gi]", 51.0, 0.0, 2.0),
            fwf(0, "=", 53.0, 0.0, 2.0),
            fwf(0, "decompose(E,g;Θ,P);", 55.0, 0.0, 2.0),
            fw(1, "HuggingGPT", 8.0, 0.0, 2.0),
            fw(2, "[Shen", 8.0, 3.0, 2.0),
            fw(3, "et", 8.0, 6.0, 2.0),
            fw(4, "al.", 8.0, 9.0, 2.0),
        ];
        let steps = sentence_walk(&page0, 0, None, Some(&page1), 0);
        assert_eq!(steps.len(), 2);
        let copied = copy_steps(
            &Selection {
                layer: Layer::Body,
                formula: false,
                formula_score: 0.0,
                steps,
            },
            |page| {
                if page == 0 {
                    Some(&page0)
                } else if page == 1 {
                    Some(&page1)
                } else {
                    None
                }
            },
        )
        .unwrap();
        assert!(copied.contains("HuggingGPT"), "{copied:?}");
        assert!(copied.contains("al."), "{copied:?}");
        assert!(
            !copied.contains("decompose"),
            "表格公式不应进入句子:{copied:?}"
        );
    }

    #[test]
    fn sentence_walk_skips_figure_label_columns_before_right_text() {
        // 图注标签列有 2 行小字(高 0.8),不是正文尺寸,续接时应跳过
        let flat = vec![
            fw(0, "We", 10.0, 0.0, 2.0),
            fw(0, "have", 14.0, 0.0, 2.0),
            fw(1, "combined", 10.0, 5.0, 2.0),
            fw(1, "with", 20.0, 5.0, 2.0),
            fw(2, "Pla", 60.0, 0.0, 0.8),
            fw(3, "Ability", 60.0, 2.0, 0.8),
            fw(4, "deep", 51.0, 0.0, 2.0),
            fw(4, "models,", 57.0, 0.0, 2.0),
            fw(5, "model.", 51.0, 5.0, 2.0),
        ];
        let steps = sentence_walk(&flat, 3, None, None, 0);
        assert_eq!(steps.len(), 2, "跳过图注标签列,续到右栏正文");
        assert_eq!((steps[0].lo, steps[0].hi), (0, 3));
        assert_eq!((steps[1].lo, steps[1].hi), (6, 8));
        let copied = copy_steps(
            &Selection {
                layer: Layer::Body,
                formula: false,
                formula_score: 0.0,
                steps,
            },
            |_| Some(&flat),
        )
        .unwrap();
        assert!(!copied.contains("Pla"), "{copied:?}");
    }

    #[test]
    fn sentence_walk_crosses_to_next_page_leftmost() {
        let page0 = vec![
            fw(0, "The", 10.0, 0.0, 2.0),
            fw(1, "sentence", 10.0, 5.0, 2.0),
            fw(2, "continues", 10.0, 10.0, 2.0),
        ];
        let page1 = vec![
            fw(0, "here.", 10.0, 0.0, 2.0),
            fw(1, "more", 10.0, 5.0, 2.0),
        ];
        let steps = sentence_walk(&page0, 2, None, Some(&page1), 0);
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].page, 0);
        assert_eq!(steps[1].page, 1);
        assert_eq!(steps[1].hi, 0);
    }

    #[test]
    fn paragraph_walk_continues_to_right_column_when_at_column_bottom() {
        let flat = vec![
            fw(0, "P1", 10.0, 0.0, 2.0),
            fw(1, "P2", 10.0, 2.5, 2.0),
            fw(2, "P3", 10.0, 5.0, 2.0),
            fw(3, "R1", 51.0, 0.0, 2.0),
            fw(4, "R2", 51.0, 2.5, 2.0),
        ];
        let steps = paragraph_walk(&flat, 1, None, None, 0);
        assert_eq!(steps.len(), 2, "左栏段落在栏底,续接右栏");
        assert_eq!((steps[0].lo, steps[0].hi), (0, 2));
        assert_eq!((steps[1].lo, steps[1].hi), (3, 4));
    }

    #[test]
    fn paragraph_walk_stops_when_right_column_indented() {
        let flat = vec![
            fw(0, "P1", 10.0, 0.0, 2.0),
            fw(1, "P2", 10.0, 2.5, 2.0),
            fw(2, "R1", 52.5, 0.0, 2.0),
            fw(3, "R2", 51.0, 2.5, 2.0),
        ];
        let steps = paragraph_walk(&flat, 0, None, None, 0);
        assert_eq!(steps.len(), 1, "右栏首行缩进,段落止于左栏");
    }

    #[test]
    fn paragraph_walk_skips_figure_label_column_between_text_columns() {
        let flat = vec![
            fw(0, "P1", 10.0, 0.0, 2.0),
            fw(1, "P2", 10.0, 2.5, 2.0),
            fw(2, "P3", 10.0, 5.0, 2.0),
            fw(3, "Pla", 60.0, 0.0, 0.8),
            fw(4, "Ability", 60.0, 2.0, 0.8),
            fw(5, "R1", 51.0, 0.0, 2.0),
            fw(6, "R2", 51.0, 2.5, 2.0),
        ];
        let steps = paragraph_walk(&flat, 1, None, None, 0);
        assert_eq!(steps.len(), 2, "段落跳过图注标签列,续到右栏正文");
        assert_eq!((steps[0].lo, steps[0].hi), (0, 2));
        assert_eq!((steps[1].lo, steps[1].hi), (5, 6));
    }

    #[test]
    fn paragraph_walk_continues_backward_to_left_column_when_right_starts_unindented() {
        let flat = vec![
            fw(0, "L1", 10.0, 0.0, 2.0),
            fw(1, "L2", 10.0, 2.5, 2.0),
            fw(2, "L3", 10.0, 5.0, 2.0),
            fw(3, "R1", 51.0, 0.0, 2.0),
            fw(4, "R2", 51.0, 2.5, 2.0),
        ];
        let steps = paragraph_walk(&flat, 4, None, None, 0);
        assert_eq!(steps.len(), 2, "右栏锚点应向前包含左栏段落");
        assert_eq!((steps[0].lo, steps[0].hi), (0, 2));
        assert_eq!((steps[1].lo, steps[1].hi), (3, 4));
    }

    #[test]
    fn paragraph_walk_does_not_continue_backward_when_right_column_indented() {
        let flat = vec![
            fw(0, "L1", 10.0, 0.0, 2.0),
            fw(1, "L2", 10.0, 2.5, 2.0),
            fw(2, "R1", 52.5, 0.0, 2.0),
            fw(3, "R2", 51.0, 2.5, 2.0),
        ];
        let steps = paragraph_walk(&flat, 3, None, None, 0);
        assert_eq!(steps.len(), 1, "右栏首行缩进,视为新段落,不向前续");
    }

    #[test]
    fn paragraph_walk_crosses_to_next_page_same_column() {
        let page0 = vec![
            fw(0, "P1", 10.0, 0.0, 2.0),
            fw(1, "P2", 10.0, 2.5, 2.0),
            fw(2, "P3", 10.0, 5.0, 2.0),
        ];
        let page1 = vec![fw(0, "P4", 10.0, 0.0, 2.0), fw(1, "P5", 10.0, 2.5, 2.0)];
        let steps = paragraph_walk(&page0, 1, None, Some(&page1), 0);
        assert_eq!(steps.len(), 2, "正文段落跨页续接");
        assert_eq!(steps[0].page, 0);
        assert_eq!(steps[1].page, 1);
        assert_eq!(steps[1].hi, 1);
    }

    #[test]
    fn classify_words_splits_body_and_small() {
        let overlay = vec![
            overlay_line(vec![("Hello".into(), 10.0, 0.0, 20.0, 2.0)]),
            overlay_line(vec![("World".into(), 10.0, 5.0, 20.0, 2.0)]),
            overlay_line(vec![("More".into(), 10.0, 10.0, 20.0, 2.0)]),
            overlay_line(vec![("footnote".into(), 10.0, 10.0, 30.0, 1.0)]),
            overlay_line(vec![("a".into(), 3.0, 0.0, 0.5, 1.0)]),
        ];
        let (body, small) = classify_words(&overlay);
        let body_texts: Vec<&str> = body.iter().map(|w| w.text.as_str()).collect();
        let small_texts: Vec<&str> = small.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(body_texts, vec!["Hello", "World", "More"]);
        assert_eq!(small_texts, vec!["footnote", "a"]);
    }

    #[test]
    fn copy_steps_joins_steps_with_newline() {
        let sel = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            steps: vec![
                SelectionStep {
                    page: 0,
                    lo: 0,
                    hi: 2,
                    column_left: Some(10.0),
                },
                SelectionStep {
                    page: 1,
                    lo: 0,
                    hi: 2,
                    column_left: Some(8.0),
                },
            ],
        };
        let page0 = vec![
            fw(0, "a", 10.0, 0.0, 2.0),
            fw(0, " ", 13.0, 0.0, 2.0),
            fw(0, "b.", 15.0, 0.0, 2.0),
        ];
        let page1 = vec![
            fw(0, "c", 8.0, 0.0, 2.0),
            fw(0, " ", 12.0, 0.0, 2.0),
            fw(0, "d.", 14.0, 0.0, 2.0),
        ];
        let mut pages = HashMap::new();
        pages.insert(0u32, page0);
        pages.insert(1u32, page1);
        let text = copy_steps(&sel, |p| pages.get(&p).map(|v| v.as_slice())).unwrap();
        assert_eq!(text, "a b.\nc d.");
    }

    #[test]
    fn translation_input_plain_text_has_no_placeholders() {
        let flat = vec![
            fw(0, "As", 0.0, 0.0, 2.0),
            fw(0, " ", 2.0, 0.0, 2.0),
            fw(0, "shown,", 3.0, 0.0, 2.0),
            fw(1, "models", 0.0, 5.0, 2.0),
        ];
        let sel = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            steps: vec![SelectionStep {
                page: 1,
                lo: 0,
                hi: 3,
                column_left: None,
            }],
        };
        let (text, formulas) = selection_translation_input(&sel, |_| Some(&flat)).unwrap();
        assert_eq!(text, "As shown,\nmodels");
        assert!(formulas.is_empty());
    }

    #[test]
    fn translation_input_merges_formula_run_into_single_placeholder() {
        let flat = vec![
            fw(0, "where", 0.0, 0.0, 2.0),
            fw(0, " ", 2.0, 0.0, 2.0),
            fwf(0, "p0", 3.0, 0.0, 2.0),
            fw(0, " ", 5.0, 0.0, 2.0),
            fwf(0, "=", 6.0, 0.0, 2.0),
            fw(0, " ", 7.0, 0.0, 2.0),
            fwf(0, "plan(E,g;Θ,P);", 8.0, 0.0, 2.0),
            fw(1, "is", 0.0, 5.0, 2.0),
        ];
        let sel = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            steps: vec![SelectionStep {
                page: 1,
                lo: 0,
                hi: 7,
                column_left: None,
            }],
        };
        let (text, formulas) = selection_translation_input(&sel, |_| Some(&flat)).unwrap();
        assert_eq!(text, "where [公式1]\nis");
        assert_eq!(formulas, vec!["p0 = plan(E,g;Θ,P);"]);
    }

    #[test]
    fn translation_input_pure_formula_is_all_placeholders() {
        let flat = vec![
            fwf(0, "p", 0.0, 0.0, 2.0),
            fwf(0, "*", 1.0, 0.0, 2.0),
            fwf(0, "=select(...);", 2.0, 0.0, 2.0),
        ];
        let sel = Selection {
            layer: Layer::Body,
            formula: true,
            formula_score: 1.0,
            steps: vec![SelectionStep {
                page: 1,
                lo: 0,
                hi: 2,
                column_left: None,
            }],
        };
        let (text, formulas) = selection_translation_input(&sel, |_| Some(&flat)).unwrap();
        assert_eq!(text, "[公式1]");
        assert_eq!(formulas, vec!["p*=select(...);"]);
        assert!(!selection_has_plain_text(&sel, |_| Some(&flat)));
    }

    #[test]
    fn translation_has_plain_text_detects_mixed_selection() {
        let flat = vec![fw(0, "text", 0.0, 0.0, 2.0), fwf(0, "x_i", 3.0, 0.0, 2.0)];
        let sel = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            steps: vec![SelectionStep {
                page: 1,
                lo: 0,
                hi: 1,
                column_left: None,
            }],
        };
        assert!(selection_has_plain_text(&sel, |_| Some(&flat)));
    }

    #[test]
    fn paragraph_range_stays_in_column_and_stops_at_gap() {
        let flat = vec![
            fw(0, "L1", 5.0, 10.0, 2.0),
            fw(1, "L2", 5.0, 12.5, 2.0),
            fw(2, "L3", 5.0, 20.0, 2.0),
            fw(3, "R1", 52.0, 10.0, 2.0),
            fw(4, "R2", 52.0, 12.5, 2.0),
        ];
        assert_eq!(paragraph_range(&flat, 1), (0, 1), "L1..L2 同段");
        assert_eq!(paragraph_range(&flat, 2), (2, 2), "L3 空行后另起段");
        assert_eq!(paragraph_range(&flat, 3), (3, 4), "右栏不并入左栏");
    }

    #[test]
    fn paragraph_indent_starts_new_paragraph() {
        let flat = vec![
            fw(0, "prev.", 8.9, 20.0, 1.0),
            fw(1, "indent", 10.5, 21.5, 1.0),
        ];
        assert_eq!(paragraph_range(&flat, 0), (0, 0));
        assert_eq!(paragraph_range(&flat, 1), (1, 1));
    }

    #[test]
    fn mid_sentence_indent_is_not_a_paragraph_boundary() {
        let flat = vec![
            fw(0, "ties", 8.9, 20.0, 1.0),
            fw(1, "The", 10.5, 21.5, 1.0),
            fw(2, "rest.", 8.9, 23.0, 1.0),
        ];
        assert_eq!(paragraph_range(&flat, 0), (0, 2), "句中续行缩进仍属同一段");
        assert_eq!(paragraph_range(&flat, 2), (0, 2));
    }

    #[test]
    fn copy_text_skips_line_end_spaces() {
        let flat = vec![
            fw(0, "Hello", 0.0, 0.0, 1.0),
            fw(0, " ", 2.0, 0.0, 1.0),
            fw(0, "world", 3.0, 0.0, 1.0),
            fw(0, " ", 7.0, 0.0, 1.0),
            fw(1, "Next", 0.0, 5.0, 1.0),
        ];
        assert_eq!(copy_text_filtered(&flat, 0, 4, None), "Hello world\nNext");
    }

    #[test]
    fn selection_rects_ignore_stray_spaces() {
        let flat = vec![
            fw(0, "Hello", 10.0, 10.0, 1.0),
            fw(0, " ", 70.0, 10.0, 1.0),
            fw(0, "world", 16.0, 10.0, 1.0),
        ];
        let rects = selection_rects_filtered(&flat, 0, 2, None);
        assert_eq!(rects.len(), 1);
        assert!((rects[0].left_cqw - 10.0).abs() < 1e-9);
        assert!((rects[0].width_cqw - 7.0).abs() < 1e-9);
    }

    #[test]
    fn snap_to_word_skips_spaces() {
        let flat = vec![
            fw(0, "Hello", 0.0, 0.0, 1.0),
            fw(0, " ", 2.0, 0.0, 1.0),
            fw(0, "world", 3.0, 0.0, 1.0),
        ];
        assert_eq!(snap_to_word(&flat, 1), 0);
        assert_eq!(snap_to_word(&flat, 2), 2);
    }

    #[test]
    fn space_words_snap_gesture_index() {
        use crate::pdf::OverlayWord;

        let line = OverlayLine {
            top_pct: 0.0,
            height_pct: 0.0,
            height_cqw: 1.0,
            words: vec![
                OverlayWord {
                    text: "Hello".into(),
                    left_pct: 0.0,
                    top_cqw: 0.0,
                    width_cqw: 1.0,
                    height_cqw: 1.0,
                },
                OverlayWord {
                    text: " ".into(),
                    left_pct: 2.0,
                    top_cqw: 0.0,
                    width_cqw: 0.1,
                    height_cqw: 1.0,
                },
                OverlayWord {
                    text: "world".into(),
                    left_pct: 3.0,
                    top_cqw: 0.0,
                    width_cqw: 1.0,
                    height_cqw: 1.0,
                },
            ],
        };
        let flat = build_flat(&[line]);
        assert_eq!(flat.len(), 3);
        assert_eq!(flat[0].gesture, 0);
        assert_eq!(flat[1].gesture, 0, "空格吸附到前一个词");
        assert_eq!(flat[2].gesture, 2);
    }

    #[test]
    fn scan_word_boundary_respects_bracket_depth_and_semicolon() {
        let mut d = 1; // 已在括号内
        assert!(!scan_word_boundary("g;", &mut d, None), "括号内分号不打断");
        assert_eq!(d, 1);
        assert!(!scan_word_boundary("Θ,P)", &mut d, None));
        assert_eq!(d, 0);
        assert!(scan_word_boundary(";", &mut d, None), "深度 0 的分号打断");

        let mut d = 0;
        assert!(scan_word_boundary("X；", &mut d, None), "全角分号同样打断");
        assert!(scan_word_boundary("done.", &mut d, None), "句号仍打断");
        assert!(
            !scan_word_boundary("al.", &mut d, Some(",")),
            "引文里的 al. 不打断"
        );
        assert!(
            scan_word_boundary("al.", &mut d, Some("HuggingGPT")),
            "句子末尾的 et al. 打断"
        );

        let mut d = 0;
        assert!(!scan_word_boundary("word", &mut d, None));
    }

    #[test]
    fn formula_sentence_walk_stops_at_line_semicolon() {
        let flat = vec![
            fwf(0, "p0", 51.0, 0.0, 2.0),
            fwf(0, "=", 53.0, 0.0, 2.0),
            fwf(0, "plan(E,g;Θ,P);", 55.0, 0.0, 2.0),
            fwf(1, "ri=reflect(E,g;Θ,P);", 51.0, 3.0, 2.0),
            fwf(2, "m=retrieve(E,g;M);", 51.0, 6.0, 2.0),
        ];
        let steps = formula_sentence_walk(&flat, 1, 0);
        assert_eq!(steps.len(), 1, "公式句不跨行/跨页");
        assert_eq!((steps[0].lo, steps[0].hi), (0, 2));
    }

    #[test]
    fn formula_sentence_walk_keeps_internal_semicolon_in_formula() {
        let flat = vec![
            fwf(0, "p=", 51.0, 0.0, 2.0),
            fwf(0, "plan(E,g;", 53.0, 0.0, 2.0),
            fwf(0, "Θ,P);", 55.0, 0.0, 2.0),
        ];
        let steps = formula_sentence_walk(&flat, 1, 0);
        assert_eq!(steps.len(), 1);
        assert_eq!((steps[0].lo, steps[0].hi), (0, 2), "整条公式一次选中");
    }

    #[test]
    fn formula_block_range_selects_fragments_within_row_band() {
        let flat = vec![
            fwf(0, "p", 51.0, 0.0, 1.5),
            fwf(1, "∗", 53.0, 0.2, 1.5),
            fwf(2, "=select(E,g,P;Θ,F).", 51.0, 1.8, 2.0),
            fwf(3, "ri=reflect(E,g,π;Θ,P);", 51.0, 6.0, 2.0),
        ];
        let (lo, hi) = formula_block_range(&flat, 1);
        assert_eq!((lo, hi), (0, 2), "同一行带的碎片整组选中,不跨表格行");
    }

    #[test]
    fn formula_block_range_covers_lines_outside_flat_order() {
        // top 顺序与 flat 顺序不同(碎片行交叠),选区必须覆盖整组行
        let flat = vec![
            fwf(0, "p", 51.0, 71.45, 1.0),
            fwf(1, "i", 51.0, 70.75, 0.7),
            fwf(2, "=", 51.0, 71.55, 0.5),
            fwf(3, "sub-plan", 51.0, 70.75, 1.5),
        ];
        let (lo, hi) = formula_block_range(&flat, 0);
        assert_eq!((lo, hi), (0, 3), "整条公式(含内容顺序靠后的行)全部选中");
    }

    #[test]
    fn line_columns_use_adjacent_gap_not_cumulative_drift() {
        let flat = vec![
            fw(0, "a", 51.0, 0.0, 2.0),
            fw(1, "b", 52.9, 3.0, 2.0),
            fw(2, "c", 54.7, 6.0, 2.0),
        ];
        let (col_of, _) = line_columns(&flat);
        assert_eq!(
            col_of.get(&0),
            col_of.get(&2),
            "相邻间距 <2cqw 的行属于同一列(公式对齐右移不拆列)"
        );
    }

    #[test]
    fn calibrated_column_gap_splits_tight_columns() {
        let flat = vec![
            fw(0, "a", 10.0, 0.0, 2.0),
            fw(1, "b", 11.5, 3.0, 2.0),
            fw(2, "c", 13.0, 6.0, 2.0),
        ];
        let (col_of_default, _) = line_columns(&flat);
        assert_eq!(
            col_of_default.get(&0),
            col_of_default.get(&2),
            "默认列间距 2.0 会把 1.5cqw 的两栏合并"
        );
        let (col_of_cal, _) = line_columns_with(&flat, 1.0);
        assert_ne!(
            col_of_cal.get(&0),
            col_of_cal.get(&2),
            "校准列间距 1.0 时应分成两栏"
        );
    }

    #[test]
    fn calibrated_small_ratio_keeps_mid_size_lines_in_body() {
        let overlay = vec![
            overlay_line(vec![("Hello".into(), 10.0, 0.0, 20.0, 2.0)]),
            overlay_line(vec![("World".into(), 10.0, 3.0, 20.0, 2.0)]),
            overlay_line(vec![("More".into(), 10.0, 6.0, 20.0, 2.0)]),
            overlay_line(vec![("Text".into(), 10.0, 9.0, 20.0, 2.0)]),
            overlay_line(vec![("note".into(), 10.0, 12.0, 20.0, 1.6)]),
        ];
        let (_, small_default) = classify_words(&overlay);
        assert!(
            small_default.iter().any(|w| w.text == "note"),
            "默认小字比例 0.91 把 0.8×中位行高判为小字"
        );
        let cal = crate::calibration::DocCalibration {
            small_height_ratio: 0.75,
            ..Default::default()
        };
        let (body_cal, small_cal) = classify_words_with(&overlay, cal);
        assert!(body_cal.iter().any(|w| w.text == "note"));
        assert!(!small_cal.iter().any(|w| w.text == "note"));
    }

    #[test]
    fn calibrated_vertical_gap_ratio_merges_formula_rows() {
        let flat = vec![
            fwf(0, "p=", 51.0, 0.0, 2.0),
            fwf(1, "plan(E,g;Θ,P);", 51.0, 3.6, 2.0),
        ];
        let (lo_default, hi_default) = formula_block_range(&flat, 0);
        assert_eq!((lo_default, hi_default), (0, 0), "默认 0.6 比例会断块");
        let cal = crate::calibration::DocCalibration {
            vertical_gap_ratio: 0.9,
            ..Default::default()
        };
        let (lo_cal, hi_cal) = formula_block_range_with(&flat, 0, cal);
        assert_eq!((lo_cal, hi_cal), (0, 1), "校准 0.9 比例保持同行");
    }

    #[test]
    fn calibrated_paragraph_indent_splits_small_indent() {
        let flat = vec![fw(0, "prev.", 8.0, 0.0, 2.0), fw(1, "next", 8.8, 3.0, 2.0)];
        let (lo_default, hi_default) = paragraph_range(&flat, 1);
        assert_eq!((lo_default, hi_default), (0, 1), "默认缩进 1.0 会合并");
        let cal = crate::calibration::DocCalibration {
            paragraph_indent_cqw: 0.5,
            ..Default::default()
        };
        let (lo_cal, hi_cal) = paragraph_range_with(&flat, 1, cal);
        assert_eq!((lo_cal, hi_cal), (1, 1), "校准缩进 0.5 识别为新段落");
    }

    #[test]
    fn formula_block_crosses_to_next_page_same_column() {
        let page0 = vec![
            fwf(0, "p=plan(E,g;Θ,P);", 51.0, 0.0, 2.0),
            fwf(1, "ri=reflect(E,g;Θ,P);", 51.0, 3.0, 2.0),
        ];
        let page1 = vec![fwf(0, "m=retrieve(E,g;M);", 51.0, 0.0, 2.0)];
        let steps = paragraph_walk(&page0, 0, None, Some(&page1), 0);
        assert_eq!(steps.len(), 2, "公式块按同列跨页续接");
        assert_eq!(steps[0].page, 0);
        assert_eq!(steps[1].page, 1);
        assert_eq!(steps[1].hi, 0);
    }

    #[test]
    fn formula_block_does_not_cross_when_next_page_starts_with_body() {
        let page0 = vec![
            fwf(0, "p=plan(E,g;Θ,P);", 51.0, 0.0, 2.0),
            fwf(1, "ri=reflect(E,g;Θ,P);", 51.0, 3.0, 2.0),
        ];
        let page1 = vec![fw(0, "Body text", 51.0, 0.0, 2.0)];
        let steps = paragraph_walk(&page0, 0, None, Some(&page1), 0);
        assert_eq!(steps.len(), 1, "下一页首行不是公式则不跨页");
    }

    #[test]
    fn classify_words_keeps_formula_fragments_and_vertical_text() {
        let overlay = vec![
            overlay_line(vec![("a".into(), 3.0, 0.0, 0.4, 2.0)]),
            overlay_line(vec![("r".into(), 3.0, 5.0, 0.4, 2.0)]),
            overlay_line(vec![("X".into(), 3.0, 10.0, 0.4, 2.0)]),
            overlay_line(vec![("Hello".into(), 10.0, 0.0, 30.0, 2.0)]),
            overlay_line(vec![("World".into(), 10.0, 3.0, 30.0, 2.0)]),
            overlay_line(vec![("More".into(), 10.0, 6.0, 30.0, 2.0)]),
            overlay_line(vec![("Text".into(), 10.0, 9.0, 30.0, 2.0)]),
            overlay_line(vec![("p".into(), 51.0, 0.0, 0.5, 1.5)]),
            overlay_line(vec![("∗".into(), 53.0, 0.2, 0.5, 1.5)]),
            overlay_line(vec![("=plan(E,g;Θ,P);".into(), 51.0, 1.8, 10.0, 2.0)]),
            overlay_line(vec![("ri=reflect(E,g;Θ,P);".into(), 51.0, 4.5, 10.0, 2.0)]),
        ];
        let (body, small) = classify_words(&overlay);
        let small_texts: Vec<&str> = small.iter().map(|w| w.text.as_str()).collect();
        assert_eq!(small_texts, vec!["a", "r", "X"], "竖排列进小字层");
        let body_formula: Vec<&str> = body
            .iter()
            .filter(|w| w.formula)
            .map(|w| w.text.as_str())
            .collect();
        assert_eq!(
            body_formula,
            vec!["p", "∗", "=plan(E,g;Θ,P);", "ri=reflect(E,g;Θ,P);"],
            "公式碎片留在正文层并标记为公式"
        );
        assert!(
            body.iter()
                .filter(|w| w.formula)
                .all(|w| w.formula_score >= FORMULA_SCORE_THRESHOLD),
            "公式词置信度应不低于阈值"
        );
        assert!(
            body.iter()
                .filter(|w| !w.formula)
                .all(|w| w.formula_score < FORMULA_SCORE_THRESHOLD),
            "正文词置信度应低于阈值"
        );
    }

    #[test]
    fn formula_score_uses_isolation_and_centering_for_display_formula() {
        // 同一正文列里:普通句子 vs 被空行隔开、略缩进的独立公式行。
        let overlay = vec![
            overlay_line(vec![
                ("if".into(), 10.0, 0.0, 3.0, 2.0),
                ("x".into(), 14.0, 0.0, 1.0, 2.0),
                (">".into(), 16.0, 0.0, 1.0, 2.0),
                ("0".into(), 18.0, 0.0, 1.0, 2.0),
                ("and".into(), 21.0, 0.0, 3.0, 2.0),
                ("y".into(), 26.0, 0.0, 1.0, 2.0),
                ("<".into(), 28.0, 0.0, 1.0, 2.0),
                ("1".into(), 30.0, 0.0, 1.0, 2.0),
            ]),
            overlay_line(vec![
                ("p0".into(), 12.0, 12.0, 2.0, 2.0),
                ("=".into(), 15.0, 12.0, 1.0, 2.0),
                ("plan(E,g;Θ,P);".into(), 17.0, 12.0, 15.0, 2.0),
            ]),
            overlay_line(vec![
                ("as".into(), 10.0, 22.0, 3.0, 2.0),
                ("required.".into(), 14.0, 22.0, 8.0, 2.0),
            ]),
        ];
        let (body, _) = classify_words(&overlay);
        let formula_words: Vec<&str> = body
            .iter()
            .filter(|w| w.formula)
            .map(|w| w.text.as_str())
            .collect();
        let text_words: Vec<&str> = body
            .iter()
            .filter(|w| !w.formula)
            .map(|w| w.text.as_str())
            .collect();
        assert_eq!(
            formula_words,
            vec!["p0", "=", "plan(E,g;Θ,P);"],
            "独立公式行由上下文置信度识别为公式"
        );
        assert_eq!(
            text_words,
            vec!["if", "x", ">", "0", "and", "y", "<", "1", "as", "required."]
        );
        assert!(
            body.iter()
                .filter(|w| w.formula)
                .all(|w| w.formula_score >= FORMULA_SCORE_THRESHOLD)
        );
        assert!(
            body.iter()
                .filter(|w| !w.formula)
                .all(|w| w.formula_score < FORMULA_SCORE_THRESHOLD)
        );
    }

    #[test]
    fn selection_bbox_and_cache_key_are_stable() {
        let flat = vec![fw(0, "p=", 10.0, 5.0, 2.0), fw(0, "x", 13.0, 5.0, 2.0)];
        let step = SelectionStep {
            page: 1,
            lo: 0,
            hi: 1,
            column_left: Some(10.0),
        };
        let bbox = selection_bbox_px(&flat, &step, 612.0);
        let key = formula_copy_key("book-1", 1, bbox);
        assert_eq!(key, formula_copy_key("book-1", 1, bbox));
        assert_ne!(key, formula_copy_key("book-2", 1, bbox));
        assert!(bbox.2 >= 1 && bbox.3 >= 1);
    }
}
