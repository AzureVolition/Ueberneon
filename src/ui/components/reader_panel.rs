// ── 全屏阅读器(连续滚动 + 双文字层) ──
//
// 连续滚动浏览 PDF;每页文字拆成「正文层」与「小字层」(脚注/角标/
// 竖排侧边小字),两层交互逻辑一致(拖动/双击/三击/Cmd+双击/Cmd+C),
// 但拖动禁止跨层。句子/段落在本层内按列过滤,未结束时可向后跨 1 页。

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use dioxus::desktop::use_window;
use dioxus::prelude::*;

use crate::formula_ocr::SingleSlotCache;
use crate::agent::{ActionMode, AgentMode};
use crate::db::metadata::agent_config::AgentConfigRow;
use crate::db::metadata::conversation::ConversationRow;
use crate::model::{BookCitation, ChatMessage, Role, UiMessage};
use crate::pdf::pdfium::{self, PdfDocument};
use crate::pdf::{OverlayLine, parse_book};
use crate::ui::components::chat_panel::ChatPanel;
use crate::ui::components::dropdown::{Dropdown, DropdownOption};
use crate::ui::components::error::{ErrorInfo, ErrorSeverity, ErrorSignal, ErrorSource};
use crate::ui::components::input_bar::InputBar;
use crate::ui::state::ConversationRuntime;
use tokio_util::sync::CancellationToken;

/// 解释对话消息渲染用 Markdown → HTML(与主聊天一致)。
fn reader_markdown_to_html(md: &str) -> String {
    let parser = pulldown_cmark::Parser::new_ext(md, pulldown_cmark::Options::ENABLE_TABLES);
    let mut html = String::new();
    pulldown_cmark::html::push_html(&mut html, parser);
    crate::math::render_math_in_html(&html)
}

/// 固定渲染质量:3.0 像素/点 = 216dpi。
/// Retina(2x)屏幕上 100% 宽度显示时天然超采样,文字更锐利;
/// 放大到 200% 时仍比旧版(144dpi)清晰。
const RENDER_SCALE: f32 = 3.0;
/// 停稳后开始渲染当前页的等待时间。
const RENDER_SETTLE_MS: u64 = 120;
/// 当前页渲染完成后预取的相邻页数。
const PREFETCH_RADIUS: u32 = 1;
/// 页面渲染宽度上限(px)：更大的页面按比例降采样，
/// 在保持屏幕清晰度的同时避免大版式书渲染过慢。
const MAX_RENDER_WIDTH_PX: f32 = 2400.0;
/// OCR 裁剪渲染倍率:4.0 像素/点 = 288dpi,提升公式识别精度。
const OCR_RENDER_SCALE: f32 = 4.0;
/// 内存中最多保留的已渲染页数
const MAX_CACHE_PAGES: usize = 60;
const ZOOM_MIN: u32 = 50;
const ZOOM_MAX: u32 = 400;
const ZOOM_STEP: u32 = 25;
/// 页面虚拟化：只渲染当前页前后各该数量的页，其余用等高分隔块。
const PAGE_WINDOW: u32 = 15;
/// 列聚类阈值(cqw)
/// 小字判定:真实字号低于同列中位字号的该比例(实测脚注约为正文的 0.90)
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
/// 翻译卡片打开时的基准宽度(px,与 style.css 保持一致)。
const TRANSLATION_CARD_WIDTH: f64 = 720.0;
/// 翻译卡片打开时按视口高度估算的最大高度比例(与 style.css max-height 一致)。
const TRANSLATION_CARD_MAX_HEIGHT_RATIO: f64 = 0.6;
/// 行距(pitch)超过列中位行距的该倍数时视为段落分界。
/// 用行距而不是字形盒间隙:全小写行盒高偏矮,字形盒间隙会虚高。
const PARAGRAPH_PITCH_RATIO: f64 = 1.35;

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
    /// Shift 扩展选区的锚点:选区创建时的起点词(页/层/索引)。
    anchor: Option<(u32, Layer, usize)>,
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
    /// 是否显示「解释」(已配置解释子代理且选区含正文)。
    explain_enabled: bool,
}

/// 翻译卡片状态机:加载中 / 成功译文 / 失败文案。
#[derive(Debug, Clone)]
struct TranslationCardState {
    x: f64,
    y: f64,
    status: ActionBarStatus,
    generation: u64,
    text: String,
    /// 原文（公式已回填），用于双栏对照展示。
    source_text: String,
    /// 原文按句拆分。
    source_sentences: Vec<String>,
    /// 译文按句拆分。
    translated_sentences: Vec<String>,
    /// 原文句 i → 译文句区间 [start, end]（含端点）。
    groups: Vec<(usize, usize)>,
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
    /// OCR 页的提示(未配置模型/识别失败);文本页为 None。
    warning: Option<String>,
}

struct ReaderSession {
    doc: Arc<PdfDocument>,
    page_count: u32,
    cache: HashMap<u32, RenderedPage>,
    /// 页高/宽比例的前缀和（len = page_count + 1）。
    page_ratio_prefix: Vec<f64>,
    /// 渲染 worker 的最新目标页（滚动/跳转只更新这里）。
    render_target: Option<u32>,
    /// 目标页代数：每次目标变化 +1，用于丢弃在途渲染结果。
    render_epoch: u64,
    /// 跳转后的一段时间内，滚动事件不重算当前页（防几何瞬时错位带跑窗口）。
    jump_lock_until: std::time::Instant,
    book_id: String,
    book_name: String,
    book_dir: PathBuf,
    selection: Option<Selection>,
    drag_anchor: Option<(u32, Layer, usize)>,
    dragging: bool,
    /// 是否正在 Shift 扩展选区(从 selection.anchor 向当前指针延伸)。
    shift_extending: bool,
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
    /// 是否已提示过页面 OCR 模型缺失/失败(每本书只提示一次)。
    ocr_warning_shown: bool,
}

/// 按页面宽度自适应缩放：常规页面用 RENDER_SCALE，超大页面降采样到
/// MAX_RENDER_WIDTH_PX 以内（屏幕显示宽度有限，过高分辨率无收益）。
fn render_scale_for_page(width_pt: f32) -> f32 {
    let cap = (MAX_RENDER_WIDTH_PX / width_pt.max(1.0)).clamp(1.5, RENDER_SCALE);
    cap
}

/// 渲染页面并拆分两层词表(阻塞,供 spawn_blocking 调用)。
/// PNG 优先读磁盘缓存,miss 时渲染并回写。
fn page_image(
    doc: &PdfDocument,
    book_dir: &Path,
    page_index: u32,
    scale: f32,
) -> Result<(String, f32, f32), String> {
    let (w, h) = doc.page_size(page_index).map_err(|e| format!("{e:#}"))?;
    let png = match crate::pdf::cached_page_png(book_dir, page_index + 1, scale) {
        Some(png) => png,
        None => {
            let png = doc
                .render_page_png(page_index, scale)
                .map_err(|e| format!("{e:#}"))?;
            crate::pdf::save_page_png(book_dir, page_index + 1, scale, &png);
            png
        }
    };
    let src = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&png)
    );
    Ok((src, w, h))
}

/// 构建某页的文字层（字符提取 + overlay + 分层）。
fn page_overlay(
    doc: &PdfDocument,
    book_dir: &Path,
    page_index: u32,
    w: f32,
    h: f32,
) -> Result<(Vec<FlatWord>, Vec<FlatWord>, Option<String>), String> {
    let chars = doc
        .page_text_chars(page_index)
        .map_err(|e| format!("{e:#}"))?;
    if crate::page_ocr::needs_ocr(&chars) {
        return match crate::page_ocr::overlay_for_page(book_dir, page_index + 1, doc) {
            Ok(Some(lines)) => {
                let mut body = build_flat(&lines);
                assign_gestures(&mut body);
                Ok((body, Vec::new(), None))
            }
            Ok(None) => Ok((
                Vec::new(),
                Vec::new(),
                Some(
                    "未配置页面 OCR 模型:扫描页暂不能选中文字,可在设置或工具栏配置后重试"
                        .to_string(),
                ),
            )),
            Err(e) => Ok((Vec::new(), Vec::new(), Some(format!("页面 OCR 失败:{e}")))),
        };
    }
    let overlay = crate::pdf::build_text_overlay(&chars, w as f64, h as f64);
    let (body, small) = classify_words(&overlay);
    Ok((body, small, None))
}

/// 图片 + 文字层一次完成（唯一渲染 worker 与初始打开用）。
fn render_page_with_overlay(
    doc: &PdfDocument,
    book_dir: &Path,
    page_index: u32,
) -> Result<RenderedPage, String> {
    let (w, _h) = doc.page_size(page_index).map_err(|e| format!("{e:#}"))?;
    let scale = render_scale_for_page(w);
    let (src, w_pt, h_pt) = page_image(doc, book_dir, page_index, scale)?;
    let (body, small, warning) = page_overlay(doc, book_dir, page_index, w_pt, h_pt)?;
    Ok(RenderedPage {
        src,
        body,
        small,
        w_pt,
        h_pt,
        warning,
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
    classify_words_with(overlay, crate::pdf::calibration::current())
}

fn classify_words_with(
    overlay: &[OverlayLine],
    cal: crate::pdf::calibration::DocCalibration,
) -> (Vec<FlatWord>, Vec<FlatWord>) {
    let flat = build_flat(overlay);
    // 每行真实字号(来自 PDFium FPDFText_GetFontSize,与字形包围盒高度无关)。
    let line_font_size: HashMap<usize, f64> = overlay
        .iter()
        .enumerate()
        .map(|(i, l)| (i, l.font_size_pt))
        .collect();

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
    let mut col_median_font = vec![0.0; col_count];
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
        let mut fs: Vec<f64> = lines
            .iter()
            .filter_map(|l| line_font_size.get(l).copied())
            .filter(|f| *f > 0.0)
            .collect();
        fs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        col_median_font[col] = median_sorted(&fs);

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
        let font = line_font_size.get(&line).copied().unwrap_or(0.0);
        let median_font = col_median_font[col];
        if font > 0.0 && median_font > 0.0 {
            // 真实字号明显小于列中位字号才是小字;全小写正文行字形盒
            // 偏矮但字号相同,不能被误判。
            if font < median_font * cal.small_height_ratio {
                return true;
            }
        } else if column_lines[col].len() >= 4 && height < median_h * cal.small_height_ratio {
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
    line_columns_with(flat, crate::pdf::calibration::column_gap_cqw())
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
    cal: crate::pdf::calibration::DocCalibration,
}

impl PageLayout {
    fn new_with(flat: &[FlatWord], cal: crate::pdf::calibration::DocCalibration) -> Self {
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
    let (line_top, line_height) = line_geometry(flat);
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
            if is_paragraph_break(&line_top, &line_height, flat[i].line, flat[next].line) {
                out.push('\n');
            } else {
                out.push_str(line_join_text(flat, flat[i].line, flat[next].line));
            }
        }
        i = next;
    }
    out
}

/// 每行的 top / 行高（用于判断换行是段落分界还是软换行）。
fn line_geometry(flat: &[FlatWord]) -> (HashMap<usize, f64>, HashMap<usize, f64>) {
    let mut top: HashMap<usize, f64> = HashMap::new();
    let mut height: HashMap<usize, f64> = HashMap::new();
    for w in flat {
        if w.text.trim().is_empty() {
            continue;
        }
        let e = top.entry(w.line).or_insert(f64::INFINITY);
        *e = (*e).min(w.top_cqw);
        height.entry(w.line).or_insert(w.line_height_cqw);
    }
    (top, height)
}

/// 行间垂直间隙超过行高 → 段落分界（保留换行）；否则视为软换行（合并）。
fn is_paragraph_break(
    top: &HashMap<usize, f64>,
    height: &HashMap<usize, f64>,
    line_a: usize,
    line_b: usize,
) -> bool {
    let (Some(ta), Some(tb), Some(ha), Some(hb)) = (
        top.get(&line_a),
        top.get(&line_b),
        height.get(&line_a),
        height.get(&line_b),
    ) else {
        return true;
    };
    let gap = tb - (ta + ha);
    // 1.25 容差:字形盒高度差异会让同一段的软换行间隙略超行高,
    // 但仍远小于真正的段落间距。
    gap > ha.max(*hb).max(0.6) * 1.25
}

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4e00}'..='\u{9fff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{f900}'..='\u{faff}'
    )
}

/// 软换行的连接符：中文字符之间不加空格，其余加空格。
fn line_join_text(flat: &[FlatWord], line_a: usize, line_b: usize) -> &'static str {
    let prev_last = flat
        .iter()
        .rev()
        .find(|w| w.line == line_a && !w.text.trim().is_empty())
        .and_then(|w| w.text.chars().rev().find(|c| !c.is_whitespace()))
        .unwrap_or(' ');
    let next_first = flat
        .iter()
        .find(|w| w.line == line_b && !w.text.trim().is_empty())
        .and_then(|w| w.text.chars().find(|c| !c.is_whitespace()))
        .unwrap_or(' ');
    if is_cjk(prev_last) && is_cjk(next_first) {
        ""
    } else {
        " "
    }
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
    let (line_top, line_height) = line_geometry(flat);
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
                if is_paragraph_break(&line_top, &line_height, last_line, flat[j].line) {
                    out.push('\n');
                } else {
                    out.push_str(line_join_text(flat, last_line, flat[j].line));
                }
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
            if is_paragraph_break(&line_top, &line_height, flat[i].line, flat[next].line) {
                out.push('\n');
            } else {
                out.push_str(line_join_text(flat, flat[i].line, flat[next].line));
            }
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
    formula_sentence_walk_with(flat, idx, start_page, crate::pdf::calibration::current())
}

fn formula_sentence_walk_with(
    flat: &[FlatWord],
    idx: usize,
    start_page: u32,
    cal: crate::pdf::calibration::DocCalibration,
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
        crate::pdf::calibration::current(),
    )
}

fn sentence_walk_with(
    flat: &[FlatWord],
    idx: usize,
    prev_flat: Option<&[FlatWord]>,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::pdf::calibration::DocCalibration,
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
    cal: crate::pdf::calibration::DocCalibration,
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
    // 句子不早于所在段落起点：标题/上一段即使没有句读也不算句子内容
    let (para_lo, _) = paragraph_range_with(flat, idx, cal);

    // 锚点句必须从该列第一词开始(列内前面没有边界)
    let mut depth = 0usize;
    let mut start_pos = 0usize;
    for k in 0..pos {
        if col_words[k] < para_lo {
            start_pos = k + 1;
            continue;
        }
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
    cal: crate::pdf::calibration::DocCalibration,
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
    // 句子范围不超出所在段落（标题/下一段即使没有句读也不并入）
    let (para_lo, para_hi) = paragraph_range_with(flat, idx, cal);
    let para_start_pos = col_words.iter().position(|&i| i == para_lo).unwrap_or(0);
    let para_end_pos = col_words
        .iter()
        .position(|&i| i == para_hi)
        .unwrap_or(col_words.len() - 1);

    // 起点:锚点之前最近边界之后,同时记录该位置的括号深度
    let mut depth = 0usize;
    let mut start_pos = para_start_pos;
    for k in para_start_pos..pos_in_col {
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
    for k in start_pos..=para_end_pos {
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
    // 段落在本列内结束且没有句读 → 句子止于段尾,不续接到下一列/页
    if para_end_pos < col_words.len() - 1 {
        steps.push(SelectionStep {
            page: start_page,
            lo: col_words[start_pos],
            hi: col_words[para_end_pos],
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
    paragraph_range_with(flat, idx, crate::pdf::calibration::current())
}

fn paragraph_range_with(
    flat: &[FlatWord],
    idx: usize,
    cal: crate::pdf::calibration::DocCalibration,
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

    // 列内行距中位数:用“顶到顶”的行距判断段落边界,避免字形盒高度
    // 差异(全小写行)导致间隙虚高、把同一段拆开。
    let mut pitches: Vec<f64> = col_lines
        .windows(2)
        .map(|w| line_top[&w[1]] - line_top[&w[0]])
        .collect();
    pitches.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let median_pitch = if pitches.is_empty() {
        0.001
    } else {
        pitches[(pitches.len() - 1) / 2].max(0.001)
    };

    let gap_ok = |a: usize, b: usize| -> bool {
        let pitch = line_top[&b] - line_top[&a];
        if pitch > median_pitch * PARAGRAPH_PITCH_RATIO {
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
    formula_block_range_with(flat, idx, crate::pdf::calibration::current())
}

fn formula_block_range_with(
    flat: &[FlatWord],
    idx: usize,
    cal: crate::pdf::calibration::DocCalibration,
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
        crate::pdf::calibration::current(),
    )
}

fn paragraph_walk_with(
    flat: &[FlatWord],
    idx: usize,
    prev_flat: Option<&[FlatWord]>,
    next_flat: Option<&[FlatWord]>,
    start_page: u32,
    cal: crate::pdf::calibration::DocCalibration,
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
    cal: crate::pdf::calibration::DocCalibration,
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
    cal: crate::pdf::calibration::DocCalibration,
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
    cal: crate::pdf::calibration::DocCalibration,
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
// 单击无动作;拖动选词;双击选词;三击选句;Cmd+双击选段;Cmd+C 复制;
// Shift+点击/拖动从选区锚点扩展(同层,可跨相邻 1 页)。
// 普通拖动禁止跨层/跨页;句子/段落可向后跨 1 页(同层同列)。

fn start_drag(mut session: Signal<Option<ReaderSession>>, page: u32, layer: Layer, idx: usize) {
    if let Some(inner) = session.write().as_mut() {
        inner.drag_anchor = Some((page, layer, idx));
        inner.dragging = true;
        inner.shift_extending = false;
        inner.selection = None;
        inner.action_bar = None;
        inner.translation = None;
    }
}

/// 计算 Shift 扩展后的选区:选区锚点 → 当前词(同层;跨页仅限相邻 1 页)。
/// 页面不在缓存、跨度超过 1 页或层不一致时返回 None(调用方保留原选区)。
fn shift_extend_selection(
    cache: &HashMap<u32, RenderedPage>,
    selection: &Selection,
    page: u32,
    layer: Layer,
    idx: usize,
) -> Option<Selection> {
    let anchor = selection.anchor?;
    let (apage, alayer, aidx_raw) = anchor;
    if alayer != layer || selection.layer != layer {
        return None;
    }
    let ar = cache.get(&apage)?;
    let aflat = layer_flat(ar, alayer);
    if aflat.is_empty() {
        return None;
    }
    let aidx = snap_to_word(aflat, aidx_raw).min(aflat.len() - 1);
    let aword = &aflat[aidx];

    let steps = if apage == page {
        let r = cache.get(&page)?;
        let flat = layer_flat(r, layer);
        if flat.is_empty() {
            return None;
        }
        let idx = snap_to_word(flat, idx).min(flat.len() - 1);
        vec![SelectionStep {
            page,
            lo: aidx.min(idx),
            hi: aidx.max(idx),
            column_left: None,
        }]
    } else if page == apage + 1 {
        // 向后跨一页:锚点页 aidx..末尾 + 目标页 0..idx(阅读顺序)。
        let r = cache.get(&page)?;
        let flat = layer_flat(r, layer);
        if flat.is_empty() {
            return None;
        }
        let idx = snap_to_word(flat, idx).min(flat.len() - 1);
        vec![
            SelectionStep {
                page: apage,
                lo: aidx,
                hi: aflat.len() - 1,
                column_left: None,
            },
            SelectionStep {
                page,
                lo: 0,
                hi: idx,
                column_left: None,
            },
        ]
    } else if apage == page + 1 {
        // 向前跨一页:目标页 idx..末尾 + 锚点页 0..aidx(阅读顺序)。
        let r = cache.get(&page)?;
        let flat = layer_flat(r, layer);
        if flat.is_empty() {
            return None;
        }
        let idx = snap_to_word(flat, idx).min(flat.len() - 1);
        vec![
            SelectionStep {
                page,
                lo: idx,
                hi: flat.len() - 1,
                column_left: None,
            },
            SelectionStep {
                page: apage,
                lo: 0,
                hi: aidx,
                column_left: None,
            },
        ]
    } else {
        return None; // 跨度超过 1 页:v1 不支持。
    };

    Some(Selection {
        layer,
        formula: aword.formula,
        formula_score: aword.formula_score,
        anchor: Some(anchor),
        steps,
    })
}

/// Shift+按下:已有同层选区时以选区锚点为起点立即扩展并进入 shift 拖动模式;
/// 无选区时与普通按下一致;扩展失败(跨层/跨页超限)时保留原选区。
fn begin_shift_extend(
    mut session: Signal<Option<ReaderSession>>,
    page: u32,
    layer: Layer,
    idx: usize,
) {
    let has_same_layer_selection = session
        .read()
        .as_ref()
        .and_then(|s| s.selection.as_ref())
        .map(|sel| sel.layer == layer && !sel.steps.is_empty())
        .unwrap_or(false);
    if !has_same_layer_selection {
        start_drag(session, page, layer, idx);
        return;
    }
    let extended = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let Some(sel) = inner.selection.as_ref() else {
            return;
        };
        shift_extend_selection(&inner.cache, sel, page, layer, idx)
    };
    if let Some(sel) = extended {
        if let Some(inner) = session.write().as_mut() {
            inner.shift_extending = true;
            inner.dragging = true;
            inner.drag_anchor = Some((page, layer, idx));
            inner.selection = Some(sel);
            inner.action_bar = None;
            inner.translation = None;
        }
    }
}

fn extend_drag(mut session: Signal<Option<ReaderSession>>, page: u32, layer: Layer, idx: usize) {
    if session
        .read()
        .as_ref()
        .map(|s| s.shift_extending)
        .unwrap_or(false)
    {
        let extended = {
            let guard = session.read();
            let Some(inner) = guard.as_ref() else {
                return;
            };
            let Some(sel) = inner.selection.as_ref() else {
                return;
            };
            shift_extend_selection(&inner.cache, sel, page, layer, idx)
        };
        if let Some(sel) = extended {
            if let Some(inner) = session.write().as_mut() {
                inner.drag_anchor = Some((page, layer, idx));
                inner.selection = Some(sel);
            }
        }
        return;
    }

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
                        anchor: Some((apage, alayer, aidx)),
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
        inner.shift_extending = false;
        inner.drag_anchor = None;
    }
}

fn cancel_drag(mut session: Signal<Option<ReaderSession>>) {
    if let Some(inner) = session.write().as_mut() {
        inner.dragging = false;
        inner.shift_extending = false;
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
        let translation_enabled = crate::translate::translate_agent().is_some()
            && selection_has_plain_text(sel, |p| {
                inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
            });
        let explain_enabled = crate::book_chat::configured()
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
            explain_enabled,
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

/// 一次翻译请求的快照（选区内容 + 模型配置 + 卡片位置）。
struct TranslationRequest {
    input: String,
    /// 追加到每个分块请求尾部的 `[Document: 书名 | Type: pdf]`（不参与展示）。
    document_context: String,
    formulas: Vec<String>,
    agent: crate::db::metadata::agent_config::AgentConfigRow,
    x: f64,
    y: f64,
}

/// 操作栏「翻译」：快照选区文本/公式/配置，异步请求后写入翻译卡片。
/// 卡片打开时尽量居中显示(按当前视口尺寸计算,而不是贴在操作栏下方)。
fn action_bar_translate(session: Signal<Option<ReaderSession>>, viewport: (f64, f64)) {
    let (vw, vh) = viewport;
    let card_w = TRANSLATION_CARD_WIDTH.min((vw - 48.0).max(0.0));
    let x = ((vw - card_w) / 2.0).max(0.0);
    let y = ((vh - TRANSLATION_CARD_MAX_HEIGHT_RATIO * vh) / 2.0).max(0.0);
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
        let document_context = crate::translate::document_context(&inner.book_name);
        let Some(agent) = crate::translate::translate_agent() else {
            return;
        };
        TranslationRequest {
            input,
            document_context,
            formulas,
            agent,
            x,
            y,
        }
    };
    start_translation(session, request);
}

/// 翻译卡片「重试」：用当前选区重新发起翻译，卡片位置不变。
fn retry_translation(session: Signal<Option<ReaderSession>>) {
    let request = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let Some(sel) = inner.selection.as_ref() else {
            return;
        };
        let Some(card) = inner.translation.as_ref() else {
            return;
        };
        let Some((input, formulas)) = selection_translation_input(sel, |p| {
            inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
        }) else {
            return;
        };
        if input.trim().is_empty() {
            return;
        }
        let document_context = crate::translate::document_context(&inner.book_name);
        let Some(agent) = crate::translate::translate_agent() else {
            return;
        };
        TranslationRequest {
            input,
            document_context,
            formulas,
            agent,
            x: card.x,
            y: card.y,
        }
    };
    start_translation(session, request);
}

/// 发起翻译：置 Loading 并流式拉取，完成后直接展示模型输出。
fn start_translation(mut session: Signal<Option<ReaderSession>>, request: TranslationRequest) {
    // 去掉选区文本首尾的换行/空白，避免把“回车”发给模型。
    let input = request.input.trim().to_string();
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
            x: request.x,
            y: request.y,
            status: ActionBarStatus::Loading,
            generation: translation_generation,
            text: String::new(),
            source_text: crate::translate::reinsert_formulas(&input, &request.formulas),
            source_sentences: Vec::new(),
            translated_sentences: Vec::new(),
            groups: Vec::new(),
        });
        translation_generation
    };
    spawn(async move {
        use futures::StreamExt;
        // hy-mt2 7B 即使带 [Document: ... | Type: pdf] 上下文，单块超过
        // ~500 字符仍会随机空返回（eval=1）。小块（≤280 字符）最稳，
        // 且每一块都追加文档上下文。
        const CHUNK_MAX_CHARS: usize = 280;

        // 长文本按完整句子分块，逐块翻译。
        let sentences = crate::translate::split_sentences(&input);
        let chunks = crate::translate::chunk_sentences(&sentences, CHUNK_MAX_CHARS);

        let mut set_card_text = |text: String| {
            let mut inner = session.write();
            if let Some(tc) = inner.as_mut().and_then(|s| s.translation.as_mut()) {
                if tc.generation == generation {
                    tc.text = text;
                }
            }
        };

        let mut parts: Vec<String> = Vec::new();
        let mut stream_error: Option<String> = None;

        for chunk in chunks.iter() {
            let mut chunk_text = chunk.join(" ");
            chunk_text.push_str(&request.document_context);
            let mut buf = String::new();
            let mut stream =
                match crate::translate::translation_stream(&request.agent, &chunk_text).await {
                    Ok(s) => s,
                    Err(e) => {
                        stream_error = Some(format!("{e}"));
                        break;
                    }
                };
            let mut first_chunk = true;
            loop {
                let next = if first_chunk {
                    first_chunk = false;
                    match tokio::time::timeout(std::time::Duration::from_secs(4), stream.next())
                        .await
                    {
                        Ok(chunk) => chunk,
                        Err(_) => {
                            set_card_text("模型加载中，首次响应可能需要几秒…".to_string());
                            stream.next().await
                        }
                    }
                } else {
                    stream.next().await
                };
                let Some(chunk) = next else {
                    break;
                };
                match chunk {
                    Ok(llm::Chunk::Text(t)) => {
                        buf.push_str(&t);
                        let mut display = parts.join("\n");
                        if !display.is_empty() {
                            display.push('\n');
                        }
                        display.push_str(&crate::translate::stream_visible(&buf));
                        set_card_text(display);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        stream_error = Some(format!("{e}"));
                        break;
                    }
                }
            }
            if stream_error.is_some() {
                break;
            }
            // 不做拒答判断、不自动重试：模型返回什么就展示什么。
            let t = crate::translate::finalize_translation(&buf)
                .unwrap_or_else(|_| buf.trim().to_string());
            parts.push(t);
        }

        let joined = parts.join("\n");
        let (status, text, aligned) = match stream_error {
            Some(e) => {
                let text = if joined.is_empty() {
                    e
                } else {
                    format!("{joined}\n\n{e}")
                };
                (ActionBarStatus::Error, text, None)
            }
            None => match crate::translate::finalize_translation(&joined) {
                Ok(t) => {
                    let text = crate::translate::reinsert_formulas(&t, &request.formulas);
                    // 展示的原文只含选区本身,不带追加给模型的文档上下文。
                    let source_text =
                        crate::translate::reinsert_formulas(&input, &request.formulas);
                    let source_sentences = crate::translate::split_sentences(&source_text);
                    let translated_sentences = crate::translate::split_sentences(&text);
                    let groups =
                        crate::translate::align_sentences(&source_sentences, &translated_sentences);
                    (
                        ActionBarStatus::Success,
                        text,
                        Some((source_text, source_sentences, translated_sentences, groups)),
                    )
                }
                Err(e) => (ActionBarStatus::Error, e, None),
            },
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
        if let Some((source_text, source_sentences, translated_sentences, groups)) = aligned {
            tc.source_text = source_text;
            tc.source_sentences = source_sentences;
            tc.translated_sentences = translated_sentences;
            tc.groups = groups;
        }
    });
}

// ── 操作栏「解释」──
//
// 以完整子 Agent 运行(参考 Task 工具):prompt 含选中文本(选区内公式先
// OCR 成 LaTeX)、书名与页码;子 Agent 可调用 ReadBook 查阅书中其他位置。

/// 一次解释请求的快照(选区文本 + 公式区域 + 子代理配置 + 卡片位置)。
struct ExplainRequest {
    input: String,
    /// 选区内公式的 (page_1based, @4x 像素裁剪框)。
    formula_regions: Vec<(u32, (i32, i32, i32, i32))>,
    book_id: String,
    book_name: String,
    page: u32,
}

/// 收集选区内公式运行的像素裁剪框(与 selection_translation_input 同款分组)。
fn selection_formula_regions(
    inner: &ReaderSession,
    sel: &Selection,
) -> Vec<(u32, (i32, i32, i32, i32))> {
    let mut out = Vec::new();
    for step in &sel.steps {
        let Some(r) = inner.cache.get(&step.page) else {
            continue;
        };
        let flat = layer_flat(r, sel.layer);
        if flat.is_empty() {
            continue;
        }
        let hi = step.hi.min(flat.len() - 1);
        let mut i = step.lo;
        while i <= hi {
            if !flat[i].formula {
                i += 1;
                continue;
            }
            let start = i;
            let mut j = i;
            loop {
                if j > hi {
                    break;
                }
                if flat[j].formula {
                    j += 1;
                    continue;
                }
                if flat[j].text.trim().is_empty() {
                    let mut k = j + 1;
                    let mut has_formula_after = false;
                    while k <= hi {
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
                        j = k;
                        continue;
                    }
                }
                break;
            }
            let end = j.saturating_sub(1).min(hi);
            if let Some(bbox) = formula_run_bbox(flat, start, end, r.w_pt) {
                out.push((step.page, bbox));
            }
            i = j.max(start + 1);
        }
    }
    out
}

fn formula_run_bbox(
    flat: &[FlatWord],
    lo: usize,
    hi: usize,
    page_width_pt: f32,
) -> Option<(i32, i32, i32, i32)> {
    let mut left = f64::INFINITY;
    let mut right = f64::NEG_INFINITY;
    let mut top = f64::INFINITY;
    let mut bottom = f64::NEG_INFINITY;
    for w in flat.iter().take(hi + 1).skip(lo) {
        if w.text.trim().is_empty() {
            continue;
        }
        left = left.min(w.left_cqw);
        right = right.max(w.left_cqw + w.width_cqw);
        top = top.min(w.top_cqw);
        bottom = bottom.max(w.top_cqw + w.height_cqw);
    }
    if !left.is_finite() {
        return None;
    }
    let scale = page_width_pt as f64 * OCR_RENDER_SCALE as f64;
    Some((
        ((left / 100.0) * scale).floor().max(0.0) as i32,
        ((top / 100.0) * scale).floor().max(0.0) as i32,
        (((right - left) / 100.0) * scale).ceil().max(1.0) as i32,
        (((bottom - top) / 100.0) * scale).ceil().max(1.0) as i32,
    ))
}

/// 操作栏「解释」:快照选区文本/公式区域,异步 OCR 公式成 LaTeX 后,
/// 打开内嵌解释对话面板(复用聊天管线,体验与普通对话一致)。
fn action_bar_explain(session: Signal<Option<ReaderSession>>, on_prompt: Callback<String>) {
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
        if !bar.explain_enabled {
            return;
        }
        let Some((input, _formulas)) = selection_translation_input(sel, |p| {
            inner.cache.get(&p).map(|r| layer_flat(r, sel.layer))
        }) else {
            return;
        };
        if input.trim().is_empty() {
            return;
        }
        ExplainRequest {
            input,
            formula_regions: selection_formula_regions(inner, sel),
            book_id: inner.book_id.clone(),
            book_name: inner.book_name.clone(),
            page: sel.steps[0].page,
        }
    };
    spawn(async move {
        let mut session = session;
        let mut input = request.input;
        // 选区公式 OCR 成 LaTeX(命中阅读器单槽缓存直接复用)。
        if !request.formula_regions.is_empty() {
            let (mut latex, misses, doc) = {
                let guard = session.read();
                let Some(inner) = guard.as_ref() else {
                    return;
                };
                let mut latex = vec![String::new(); request.formula_regions.len()];
                let mut misses = Vec::new();
                for (i, (page, bbox)) in request.formula_regions.iter().enumerate() {
                    let key = formula_copy_key(&inner.book_id, *page, *bbox);
                    if let Some(v) = inner.ocr_cache.get(&key) {
                        latex[i] = v.to_string();
                    } else {
                        misses.push((i, key, *page, *bbox));
                    }
                }
                (latex, misses, inner.doc.clone())
            };
            if !misses.is_empty() {
                let misses_clone = misses.clone();
                let ocr = tokio::task::spawn_blocking(move || -> Vec<Option<String>> {
                    misses_clone
                        .iter()
                        .map(|(_, _, page, bbox)| {
                            run_formula_ocr(doc.clone(), (*page as usize).saturating_sub(1), *bbox)
                                .ok()
                        })
                        .collect()
                })
                .await
                .unwrap_or_default();
                for (k, (i, key, _, _)) in misses.iter().enumerate() {
                    if let Some(v) = ocr.get(k).and_then(|v| v.as_ref()) {
                        latex[*i] = v.clone();
                        if let Some(inner) = session.write().as_mut() {
                            inner.ocr_cache.put(key.clone(), v.clone());
                        }
                    }
                }
            }
            for (i, l) in latex.iter().enumerate() {
                if !l.is_empty() {
                    input = input.replace(&format!("[公式{}]", i + 1), l);
                }
            }
        }

        let prompt = crate::explain::build_prompt(
            &input,
            &request.book_name,
            &request.book_id,
            request.page,
        );
        close_action_bar(session);
        on_prompt.call(prompt);
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
            anchor: Some((page, layer, idx)),
            steps: vec![SelectionStep {
                page,
                lo: idx,
                hi: idx,
                column_left: None,
            }],
        });
        inner.dragging = false;
        inner.shift_extending = false;
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
            anchor: Some((page, layer, anchor)),
            steps: sentence_walk(flat, anchor, prev_flat, next_flat, page),
        }
    };
    if let Some(inner) = session.write().as_mut() {
        inner.selection = Some(selection);
        inner.dragging = false;
        inner.shift_extending = false;
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
            anchor: Some((page, layer, anchor)),
            steps: paragraph_walk(flat, anchor, prev_flat, next_flat, page),
        }
    };
    if let Some(inner) = session.write().as_mut() {
        inner.selection = Some(selection);
        inner.dragging = false;
        inner.shift_extending = false;
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
    if evt.modifiers().contains(Modifiers::SHIFT)
        && session
            .read()
            .as_ref()
            .and_then(|s| s.selection.as_ref())
            .map(|sel| !sel.steps.is_empty())
            .unwrap_or(false)
    {
        return; // Shift 扩展中:点击空白不清除选区
    }
    if let Some(inner) = session.write().as_mut() {
        inner.selection = None;
        inner.shift_extending = false;
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
                                    if evt.modifiers().contains(Modifiers::SHIFT) {
                                        begin_shift_extend(session, p, layer, idx);
                                    } else {
                                        start_drag(session, p, layer, idx);
                                    }
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
                                move |evt: MouseEvent| {
                                    if evt.modifiers().contains(Modifiers::SHIFT) {
                                        evt.stop_propagation();
                                        return; // Shift 扩展由 mousedown 处理,跳过点击计数
                                    }
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
                                    if evt.modifiers().contains(Modifiers::SHIFT) {
                                        return; // Shift 扩展不覆盖为选词
                                    }
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

/// 滚动后按前缀和求当前页：第 p 页顶 = page_w*prefix[p-1] + gap*(p-1)。
fn current_page_from_prefix(
    prefix: &[f64],
    page_w: f64,
    gap: f64,
    scroll_top: f64,
    client_height: f64,
) -> u32 {
    let n = prefix.len().saturating_sub(1) as u32;
    if n == 0 {
        return 1;
    }
    let center = scroll_top + client_height / 2.0;
    let mut lo = 1u32;
    let mut hi = n;
    let mut ans = 1u32;
    while lo <= hi {
        let mid = (lo + hi) / 2;
        let top = page_w * prefix[(mid - 1) as usize] + gap * (mid - 1) as f64;
        if top <= center {
            ans = mid;
            lo = mid + 1;
        } else {
            hi = mid - 1;
        }
    }
    ans
}

/// 虚拟化窗口的顶/底分隔块高度（基于全量页高/宽比例前缀和）。
/// 打开时全部页尺寸已知，滚动高度从一开始就是完整文档高度。
fn spacer_heights(
    prefix: &[f64],
    page_w: f64,
    gap: f64,
    window_start: u32,
    window_end: u32,
) -> (f64, f64) {
    let n = prefix.len().saturating_sub(1) as u32;
    let top = if window_start > 1 {
        page_w * prefix[(window_start - 1) as usize] + gap * (window_start - 2) as f64
    } else {
        0.0
    };
    let count = n.saturating_sub(window_end);
    let bottom = if count > 0 {
        page_w * (prefix[n as usize] - prefix[window_end as usize]) + gap * (count - 1) as f64
    } else {
        0.0
    };
    (top, bottom)
}

/// 第 page 页的高/宽比例（来自前缀和差分；几何计算用）。
fn page_ratio_at(prefix: &[f64], page: u32) -> f64 {
    prefix
        .get(page as usize)
        .zip(prefix.get((page - 1) as usize))
        .map(|(hi, lo)| hi - lo)
        .unwrap_or(1.0)
}

/// 第 page 页的宽/高比例（CSS `aspect-ratio` 语义是 width/height）。
fn page_aspect_at(prefix: &[f64], page: u32) -> f64 {
    let ratio = page_ratio_at(prefix, page);
    if ratio > 0.0 { 1.0 / ratio } else { 1.0 }
}

/// 虚拟化窗口上下界（以当前页为中心，钳制到文档范围）。
fn window_bounds(current: u32, page_count: u32, radius: u32) -> (u32, u32) {
    (
        current.saturating_sub(radius).max(1),
        current.saturating_add(radius).min(page_count.max(1)),
    )
}

fn update_current_page(
    session: Signal<Option<ReaderSession>>,
    mut current_page: Signal<u32>,
    mut page_input: Signal<String>,
    mut last_width: Signal<i32>,
    scroll_top: f64,
    client_width: i32,
    client_height: i32,
    zoom_now: u32,
) {
    let guard = session.read();
    let Some(inner) = guard.as_ref() else {
        return;
    };
    // 面板展开/收起导致阅读区宽度变化时，不重算当前页，
    // 由面板切换 effect 重新跳回原页。
    if *last_width.read() != client_width {
        last_width.set(client_width);
        return;
    }
    // 跳转后的短暂窗口内不重算当前页，避免几何瞬时错位把窗口带跑。
    if std::time::Instant::now() < inner.jump_lock_until {
        return;
    }
    let content_w = (client_width as f64 - 48.0).max(1.0);
    let page_w = content_w * zoom_now as f64 / 100.0;
    let cur = current_page_from_prefix(
        &inner.page_ratio_prefix,
        page_w,
        28.0,
        scroll_top,
        client_height as f64,
    );
    if *current_page.read() != cur {
        current_page.set(cur);
        page_input.set(cur.to_string());
        crate::reading_position::save(&inner.book_dir, cur);
    }
}

/// 跳转落位：优先等目标页元素进入 DOM 后按真实 offsetTop 定位
/// （对 client_width/占位几何误差免疫）；元素迟迟未出现时退回前缀和公式。
fn scroll_to_page(
    desktop: &dioxus::desktop::DesktopContext,
    page: u32,
    ratio_top: f64,
    gap_part: f64,
    zoom: u32,
) {
    let js = format!(
        r#"(function(){{var p="{page}";var n=0;(function tick(){{var sc=document.querySelector('.reader-scroll');if(!sc){{if(n++<80){{setTimeout(tick,20);}}return;}}var el=document.querySelector('[data-page="'+p+'"]');if(el){{var top=el.offsetTop-sc.offsetTop;sc.scrollTop=top;if(Math.abs(sc.scrollTop-top)<1)return;if(n++<80){{setTimeout(tick,20);return;}}}}else{{if(n++<80){{setTimeout(tick,20);return;}}}}var pw=Math.max(sc.clientWidth-48,200)*{zoom}/100;sc.scrollTop=pw*{ratio_top}+{gap_part};}})();}})()"#
    );
    let _ = desktop.webview.evaluate_script(&js);
}

/// 滚动/跳转后更新渲染目标；目标变化使在途结果作废。
fn schedule_render(
    mut session: Signal<Option<ReaderSession>>,
    page: u32,
    mut page_loading: Signal<Option<u32>>,
) {
    if let Some(inner) = session.write().as_mut() {
        if inner.render_target != Some(page) {
            inner.render_target = Some(page);
            inner.render_epoch += 1;
            if *page_loading.read() != Some(page) {
                page_loading.set(None);
            }
        }
    }
}

/// 目录/页码跳转：立即按前缀和设置 scrollTop，渲染由唯一 worker 异步补齐。
fn request_jump(
    session: Signal<Option<ReaderSession>>,
    desktop: dioxus::desktop::DesktopContext,
    page: u32,
    mut page_loading: Signal<Option<u32>>,
    zoom: Signal<u32>,
) {
    let target = page.min({
        session
            .read()
            .as_ref()
            .map(|s| s.page_count)
            .unwrap_or(page)
    });
    let cached = session
        .read()
        .as_ref()
        .map(|s| s.cache.contains_key(&target))
        .unwrap_or(false);
    page_loading.set(Some(target));
    if cached {
        page_loading.set(None);
    }
    let (ratio_top, gap_part) = {
        let guard = session.read();
        let Some(inner) = guard.as_ref() else {
            return;
        };
        let p = target.clamp(1, inner.page_count.max(1));
        (
            inner.page_ratio_prefix[(p - 1) as usize],
            28.0 * (p - 1) as f64,
        )
    };
    if let Some(inner) = session.write_unchecked().as_mut() {
        inner.render_target = Some(target);
        inner.render_epoch += 1;
        inner.jump_lock_until = std::time::Instant::now() + std::time::Duration::from_millis(800);
    }
    scroll_to_page(&desktop, target, ratio_top, gap_part, *zoom.read());
}

/// 唯一渲染 worker：停稳后渲染当前页（图 + 文字层一次完成），
/// 再串行预取相邻页；目标变化通过 epoch 丢弃在途结果，最多浪费一页。
fn spawn_render_worker(
    session: Signal<Option<ReaderSession>>,
    mut error_signal: Signal<ErrorSignal>,
    mut page_loading: Signal<Option<u32>>,
) {
    spawn(async move {
        loop {
            let Some(page) = session.read().as_ref().and_then(|s| s.render_target) else {
                return; // session 已销毁（组件卸载）。
            };
            let epoch = session.read().as_ref().map(|s| s.render_epoch).unwrap_or(0);
            // 停稳等待：30ms 切片，目标/epoch 变化即重置。
            let mut stable = false;
            for _ in 0..(RENDER_SETTLE_MS / 30) {
                tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                let (cur, cur_epoch) = {
                    let guard = session.read();
                    let Some(inner) = guard.as_ref() else {
                        break;
                    };
                    (inner.render_target, inner.render_epoch)
                };
                if cur != Some(page) || cur_epoch != epoch {
                    break;
                }
                stable = true;
            }
            if !stable {
                continue;
            }
            let claimed = (page, epoch);
            let cached = session
                .read()
                .as_ref()
                .map(|s| s.cache.contains_key(&page))
                .unwrap_or(false);
            if !cached {
                let (doc, bd) = {
                    let guard = session.read();
                    let Some(inner) = guard.as_ref() else {
                        break;
                    };
                    (inner.doc.clone(), inner.book_dir.clone())
                };
                let result = tokio::task::spawn_blocking(move || {
                    render_page_with_overlay(&doc, &bd, page - 1)
                })
                .await;
                let mut guard = session.write_unchecked();
                let Some(inner) = guard.as_mut() else {
                    break;
                };
                let stale = inner.render_epoch != claimed.1 || inner.render_target != Some(page);
                if stale
                    && !inner
                        .render_target
                        .map(|t| page.abs_diff(t) <= PREFETCH_RADIUS)
                        .unwrap_or(false)
                {
                    continue; // 目标已变且结果不再有用，丢弃在途结果。
                }
                match result {
                    Ok(Ok(rendered)) => {
                        if let Some(warn) = rendered.warning.as_ref() {
                            if !inner.ocr_warning_shown {
                                inner.ocr_warning_shown = true;
                                error_signal.write().push(ErrorInfo::new(
                                    "reader-ocr-unavailable",
                                    "页面 OCR 不可用",
                                    warn.clone(),
                                    ErrorSeverity::Warning,
                                    ErrorSource::General,
                                ));
                            }
                        }
                        inner.cache.insert(page, rendered);
                        evict_cache(inner, inner.render_target.unwrap_or(page));
                    }
                    Ok(Err(e)) => {
                        error_signal.write().push(ErrorInfo::new(
                            "reader-render-failed",
                            "渲染页面失败",
                            e,
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                    Err(e) => {
                        error_signal.write().push(ErrorInfo::new(
                            "reader-render-failed",
                            "渲染页面失败",
                            format!("{e}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                }
                if *page_loading.read() == Some(page) {
                    page_loading.set(None);
                }
                drop(guard);
            } else if *page_loading.read() == Some(page) {
                page_loading.set(None);
            }

            // 预取相邻页：先向后一页，再向前一页；目标一变即停。
            for offset in 1..=PREFETCH_RADIUS {
                for sign in [1i32, -1i32] {
                    let p = page as i64 + sign as i64 * offset as i64;
                    if p < 1 {
                        continue;
                    }
                    let p = p as u32;
                    let job = {
                        let guard = session.read();
                        let Some(inner) = guard.as_ref() else {
                            break;
                        };
                        if p > inner.page_count
                            || inner.cache.contains_key(&p)
                            || inner.render_epoch != epoch
                            || inner.render_target != Some(page)
                        {
                            None
                        } else {
                            Some((inner.doc.clone(), inner.book_dir.clone()))
                        }
                    };
                    let Some((doc, bd)) = job else {
                        break;
                    };
                    let result = tokio::task::spawn_blocking(move || {
                        render_page_with_overlay(&doc, &bd, p - 1)
                    })
                    .await;
                    let mut guard = session.write_unchecked();
                    let Some(inner) = guard.as_mut() else {
                        break;
                    };
                    if inner.render_epoch != epoch || inner.render_target != Some(page) {
                        let still_useful = inner
                            .render_target
                            .map(|t| p.abs_diff(t) <= PREFETCH_RADIUS)
                            .unwrap_or(false);
                        if !still_useful {
                            break;
                        }
                    }
                    if let Ok(Ok(rendered)) = result {
                        if let Some(warn) = rendered.warning.as_ref() {
                            if !inner.ocr_warning_shown {
                                inner.ocr_warning_shown = true;
                                error_signal.write().push(ErrorInfo::new(
                                    "reader-ocr-unavailable",
                                    "页面 OCR 不可用",
                                    warn.clone(),
                                    ErrorSeverity::Warning,
                                    ErrorSource::General,
                                ));
                            }
                        }
                        inner.cache.insert(p, rendered);
                        evict_cache(inner, inner.render_target.unwrap_or(page));
                    }
                    drop(guard);
                }
            }

            // 一轮完成：等待目标/epoch 变化，避免空转。
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                let cur = session
                    .read()
                    .as_ref()
                    .map(|s| (s.render_target, s.render_epoch));
                if cur.is_none() {
                    return; // session 已销毁（组件卸载）。
                }
                match cur {
                    Some((Some(p), e)) if p != claimed.0 || e != claimed.1 => break,
                    _ => continue,
                }
            }
        }
    });
}

/// 缓存淘汰：超过上限时移除离当前页最远的页（保留当前选区引用的页）。
fn evict_cache_pages(
    cache: &mut HashMap<u32, RenderedPage>,
    current: u32,
    selection: Option<&Selection>,
    max: usize,
) {
    if cache.len() <= max {
        return;
    }
    let mut keep: HashSet<u32> = selection
        .map(|sel| sel.steps.iter().map(|s| s.page).collect())
        .unwrap_or_default();
    keep.insert(current);
    let mut pages: Vec<u32> = cache
        .keys()
        .copied()
        .filter(|p| !keep.contains(p))
        .collect();
    pages.sort_by_key(|p| {
        (
            std::cmp::Reverse(p.abs_diff(current)),
            std::cmp::Reverse(*p),
        )
    });
    let overflow = cache.len() - max;
    for p in pages.into_iter().take(overflow) {
        cache.remove(&p);
    }
}

fn evict_cache(inner: &mut ReaderSession, current: u32) {
    evict_cache_pages(
        &mut inner.cache,
        current,
        inner.selection.as_ref(),
        MAX_CACHE_PAGES,
    );
}

#[component]
pub fn ReaderPanel(
    book_id: String,
    project_id: Option<String>,
    initial_citation: Option<BookCitation>,
    error_signal: Signal<ErrorSignal>,
    on_back: Callback<()>,
) -> Element {
    let session = use_signal(|| Option::<ReaderSession>::None);
    let zoom = use_signal(|| 100u32);
    let desktop = use_window();
    // 滚动容器内容宽度（clientWidth - padding）。挂载/缩放/滚动时更新。
    let mut client_width = use_signal(|| 800.0f64);
    let last_scroll_width = use_signal(|| 0i32);
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
    // 翻译卡片拖动中：(指针起点 x, y, 卡片原点 x, y)
    let drag_card = use_signal(|| Option::<(f64, f64, f64, f64)>::None);
    // 翻译卡片悬停联动：(0=原文, 1=译文, 句子索引)
    let translation_hover = use_signal(|| Option::<(usize, usize)>::None);
    // 目录侧栏：是否展开 + 当前目录数据。
    let toc_open = use_signal(|| true);
    let toc = use_signal(|| Option::<crate::pdf::toc::TocFile>::None);
    // 页码：当前页（滚动更新）与输入框文本。
    let current_page = use_signal(|| 1u32);
    let page_input = use_signal(|| "1".to_string());
    // 目录中已收起的父条目 id（仅本次会话）。
    let toc_collapsed = use_signal(|| HashSet::<String>::new());
    // 目录当前高亮：(id, 所在页)。点击时立即置位；滚动到其它页后重算。
    let toc_active_id = use_signal(|| Option::<(String, u32)>::None);
    // 跳页渲染中：显示“正在加载第 N 页…”。
    let page_loading = use_signal(|| Option::<u32>::None);
    // ── 全书搜索面板 ──
    let search_open = use_signal(|| false);
    let search_query = use_signal(String::new);
    let search_results = use_signal(Vec::<(u32, String)>::new);
    let search_running = use_signal(|| false);
    let search_gen = use_signal(|| 0u64);

    // ── 书旁持久对话侧栏(完整 Agent 管线:落库 + usage + hook + 审批) ──
    let mut chat_open = use_signal(|| false);
    let mut chat_runtimes = use_signal(|| HashMap::<String, ConversationRuntime>::new());
    let chat_active_conv = use_signal(String::new);
    let mut chat_streaming = use_signal(|| false);
    let chat_streaming_projects = use_signal(Vec::<String>::new);
    let chat_approval_tx = use_signal(|| {
        HashMap::<String, tokio::sync::mpsc::Sender<(String, bool)>>::new()
    });
    let chat_streaming_states: Arc<Mutex<HashMap<String, UiMessage>>> =
        Arc::new(Mutex::new(HashMap::new()));
    let chat_action_mode = use_signal(|| ActionMode::Regular);
    let chat_agent_mode = use_signal(|| {
        crate::settings::get()
            .general
            .default_agent_mode
            .parse::<AgentMode>()
            .unwrap_or_default()
    });
    let chat_agent_config_id = use_signal(String::new);
    let chat_approval_hint = use_signal(|| Option::<String>::None);
    let chat_conversations = use_signal(Vec::<ConversationRow>::new);
    let chat_agent_config = use_signal(Vec::<AgentConfigRow>::new);
    let env_book_id = book_id.clone();
    let env_project_id = project_id.clone();
    let reading_env = use_signal(move || {
        let snapshot =
            crate::book_chat::ReadingEnvSnapshot::from_ids(&env_book_id, env_project_id.as_deref());
        crate::book_chat::ReadingEnvState::new(snapshot)
    });
    let book_id_for_handler = book_id.clone();
    let project_id_for_handler = project_id.clone();

    // 书签/对话/搜索面板切换会改变阅读区宽度：等布局稳定后跳回当前页。
    let mut panel_layout = use_signal(|| (false, false, false));
    let desktop_panel = desktop.clone();
    use_effect(move || {
        let now = (
            *chat_open.read(),
            *search_open.read(),
            *toc_open.read(),
        );
        if *panel_layout.read() == now {
            return;
        }
        panel_layout.set(now);
        let session = session;
        let desktop = desktop_panel.clone();
        let page_loading = page_loading;
        let zoom = zoom;
        let current_page = current_page;
        spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(80)).await;
            let page = current_page();
            request_jump(session, desktop, page, page_loading, zoom);
        });
    });

    // 书旁对话不再与书绑定：列出来源学习计划的对话，由用户选择。
    let source_project =
        project_id.clone().unwrap_or_else(|| crate::db::DEFAULT_PROJECT_ID.to_string());
    let source_project_init = source_project.clone();
    let mut chat_init_ran = use_signal(|| false);
    let chat_streaming_states_init = chat_streaming_states.clone();
    use_effect(move || {
        if *chat_init_ran.read() {
            return;
        }
        chat_init_ran.set(true);
        let pid = source_project_init.clone();
        let mut convs_sig = chat_conversations;
        let mut cid_sig = chat_active_conv;
        let runtimes = chat_runtimes;
        let ss = chat_streaming_states_init.clone();
        let mut ac_id = chat_agent_config_id;
        let mut ac_list = chat_agent_config;
        spawn(async move {
            let rows = crate::db::with_db(|conn| {
                crate::db::metadata::conversation::list_by_agent_config(
                    conn,
                    &pid,
                    crate::book_chat::READ_HELPER_AGENT_ID,
                )
                .unwrap_or_default()
            });
            convs_sig.set(rows.clone());
            let selected = rows.first().cloned().or_else(|| {
                // 没有对话时自动创建一个普通对话，保证侧栏可用。
                let new_id = crate::db::with_db(|conn| {
                    crate::db::metadata::conversation::create_with_status(
                        conn,
                        &pid,
                        "阅读对话",
                        None,
                        Some(crate::book_chat::READ_HELPER_AGENT_ID),
                        crate::db::metadata::conversation::ConversationStatus::SubAgent,
                    )
                })
                .unwrap_or_default();
                crate::db::with_db(|conn| {
                    crate::db::metadata::conversation::get(conn, &new_id)
                        .ok()
                        .flatten()
                })
            });
            if let Some(row) = selected {
                let cid = row.id.clone();
                cid_sig.set(cid.clone());
                crate::ui::components::app::ensure_conv_loaded(
                    &cid,
                    runtimes,
                    ss,
                    reader_markdown_to_html,
                );
                let cfg_id = row.agent_config_id.clone().unwrap_or_default();
                ac_id.set(cfg_id.clone());
                let cfg_rows = if cfg_id.is_empty() {
                    Vec::new()
                } else {
                    crate::db::with_db(|conn| {
                        crate::db::metadata::agent_config::get(conn, &cfg_id)
                            .ok()
                            .flatten()
                            .into_iter()
                            .collect()
                    })
                };
                ac_list.set(cfg_rows);
                if !rows.iter().any(|r| r.id == row.id) {
                    let mut all = rows;
                    all.push(row);
                    convs_sig.set(all);
                }
            }
        });
    });

    let chat_streaming_states_select = chat_streaming_states.clone();
    let source_project_select = source_project.clone();
    let on_select_chat_conversation = Callback::new(move |cid: String| {
        let mut cid_sig = chat_active_conv;
        let mut ac_id_sig = chat_agent_config_id;
        let mut ac_list_sig = chat_agent_config;
        let runtimes = chat_runtimes;
        let ss = chat_streaming_states_select.clone();
        let row = crate::db::with_db(|conn| {
            crate::db::metadata::conversation::get(conn, &cid)
                .ok()
                .flatten()
        });
        let Some(row) = row else {
            return;
        };
        // 阅读对话管理只允许当前学习计划的对话。
        if row.project_id != source_project_select {
            return;
        }
        cid_sig.set(cid.clone());
        crate::ui::components::app::ensure_conv_loaded(
            &cid,
            runtimes,
            ss,
            reader_markdown_to_html,
        );
        let cfg_id = row.agent_config_id.clone().unwrap_or_default();
        ac_id_sig.set(cfg_id.clone());
        let cfg_rows = if cfg_id.is_empty() {
            Vec::new()
        } else {
            crate::db::with_db(|conn| {
                crate::db::metadata::agent_config::get(conn, &cfg_id)
                    .ok()
                    .flatten()
                    .into_iter()
                    .collect()
            })
        };
        ac_list_sig.set(cfg_rows);
    });

    let on_new_chat_conversation = Callback::new(move |_| {
        let mut convs_sig = chat_conversations;
        let pid = source_project.clone();
        let new_id = crate::db::with_db(|conn| {
            crate::db::metadata::conversation::create_with_status(
                conn,
                &pid,
                "阅读对话",
                None,
                Some(crate::book_chat::READ_HELPER_AGENT_ID),
                crate::db::metadata::conversation::ConversationStatus::SubAgent,
            )
        });
        let Ok(new_id) = new_id else {
            return;
        };
        let row = crate::db::with_db(|conn| {
            crate::db::metadata::conversation::get(conn, &new_id)
                .ok()
                .flatten()
        });
        if let Some(r) = row {
            let mut all = convs_sig.read().clone();
            all.push(r.clone());
            convs_sig.set(all);
            on_select_chat_conversation.call(r.id);
        }
    });

    let on_delete_chat_conversation = Callback::new(move |_| {
        let cid = chat_active_conv();
        if cid.is_empty() {
            return;
        }
        if let Some(rt) = chat_runtimes.read().get(&cid) {
            if let Some(ref token) = rt.cancel_token {
                token.cancel();
            }
        }
        crate::book_chat::delete_conversation(&cid);
        {
            let mut rts = chat_runtimes;
            rts.write().remove(&cid);
        }
        {
            let mut atx = chat_approval_tx;
            atx.write().remove(&cid);
        }
        {
            let mut convs_sig = chat_conversations;
            let mut convs = convs_sig.read().clone();
            convs.retain(|r| r.id != cid);
            convs_sig.set(convs.clone());
            if let Some(next) = convs.first() {
                on_select_chat_conversation.call(next.id.clone());
            } else {
                on_new_chat_conversation.call(());
            }
        }
    });

    let chat_streaming_states_send = chat_streaming_states.clone();
    let send_chat_message = Callback::new(move |input: String| {
        let input = input.trim().to_string();
        if input.is_empty() {
            return;
        }
        let mut convs_sig = chat_conversations;
        let cid = chat_active_conv();
        if cid.is_empty() {
            error_signal.write().push(ErrorInfo::new(
                "book-chat-not-ready",
                "书聊尚未初始化",
                "请稍候再发送。",
                ErrorSeverity::Warning,
                ErrorSource::General,
            ));
            return;
        }
        let user_msg = ChatMessage {
            role: Role::User,
            content: input.clone(),
            timestamp: chrono::Local::now(),
            reasoning: String::new(),
            segments: Vec::new(),
            content_html: reader_markdown_to_html(&input),
        };
        // 第一条用户消息作为阅读对话标题。
        let is_first = crate::db::with_db(|conn| {
            crate::db::metadata::message::list_by_conversation(conn, &cid)
                .map(|v| v.is_empty())
                .unwrap_or(false)
        });
        if is_first {
            let title = crate::model::title_from_messages(&[user_msg.clone()]);
            crate::db::try_with_db(|conn| {
                if let Some(mut row) =
                    crate::db::metadata::conversation::get(conn, &cid).ok().flatten()
                {
                    row.title = title.clone();
                    let _ = crate::db::metadata::conversation::update(conn, &row);
                }
            });
            let mut convs = convs_sig.read().clone();
            if let Some(row) = convs.iter_mut().find(|r| r.id == cid) {
                row.title = title;
            }
            convs_sig.set(convs);
        }
        {
            let mut rt = chat_runtimes.write();
            let rt = rt.entry(cid.clone()).or_default();
            rt.messages.push(UiMessage::Static(user_msg.clone()));
            rt.tick += 1;
        }

        chat_streaming.set(true);
        let bridge_cancel = CancellationToken::new();
        chat_runtimes
            .write()
            .entry(cid.clone())
            .or_default()
            .cancel_token = Some(bridge_cancel.clone());

        let cur_action = chat_action_mode();
        let cur_mode = chat_agent_mode();
        let rt = chat_runtimes;
        let streaming = chat_streaming;
        let streaming_projects = chat_streaming_projects;
        let ss = chat_streaming_states_send.clone();
        let err_sig = error_signal;
        let atx = chat_approval_tx;
        let pid = crate::db::DEFAULT_PROJECT_ID.to_string();
        let mut env_sig = reading_env;
        let system_preamble = env_sig.write().take_preamble();
        spawn(async move {
            crate::ui::bridge::run_agent_loop(crate::ui::bridge::BridgeContext {
                user_input: input,
                system_preamble,
                action_mode: cur_action,
                agent_mode: cur_mode,
                runtimes: rt,
                is_streaming: streaming,
                streaming_project_id: streaming_projects,
                project_id: pid,
                cancel_token: bridge_cancel,
                conversation_id: cid,
                streaming_states: ss,
                error_signal: err_sig,
                approval_tx: atx,
            })
            .await;
        });
    });

    let on_explain_prompt = Callback::new(move |prompt: String| {
        chat_open.set(true);
        send_chat_message.call(prompt);
    });

    // 引用指针 → 阅读器跳页并高亮原文。
    let mut citation_target = use_signal(|| Option::<(u32, String)>::None);
    let desktop_citation = desktop.clone();
    use_effect(move || {
        let target = citation_target.read().clone();
        if let Some((page, quote)) = target {
            citation_target.set(None);
            let session = session;
            let desktop = desktop_citation.clone();
            let current_page = current_page;
            let page_input = page_input;
            let page_loading = page_loading;
            let zoom = zoom;
            spawn(async move {
                open_citation(
                    session,
                    desktop,
                    page,
                    quote,
                    current_page,
                    page_input,
                    page_loading,
                    zoom,
                );
            });
        }
    });

    let mut opened = use_signal(|| false);
    let desktop_effect = desktop.clone();
    use_effect(move || {
        if *opened.read() {
            return;
        }
        opened.set(true);
        let book_id = book_id.clone();
        let mut session = session;
        let mut err = error_signal;
        let desktop = desktop_effect.clone();
        let mut current_page = current_page;
        let mut page_input = page_input;
        let page_loading = page_loading;
        let zoom = zoom;
        let initial_citation = initial_citation.clone();
        let initial_citation_start = initial_citation.clone();
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
                let saved_page =
                    crate::reading_position::load(&book_dir).unwrap_or(1).clamp(1, page_count);
                let citation_page =
                    initial_citation_start.as_ref().map(|c| c.page).unwrap_or(saved_page);
                let start_page = citation_page.clamp(1, page_count);
                crate::pdf::prepare_page_cache(&book_dir);
                let _ = crate::pdf::calibration::ensure_for_book(&book_id, &book_dir, &doc);
                let first = render_page_with_overlay(&doc, &book_dir, start_page - 1)?;
                let mut prefix: Vec<f64> = Vec::with_capacity(page_count as usize + 1);
                prefix.push(0.0);
                for i in 0..page_count {
                    let (w, h) = doc.page_size(i).map_err(|e| format!("{e:#}"))?;
                    let last = prefix.last().copied().unwrap_or(0.0);
                    prefix.push(last + h as f64 / w.max(1.0) as f64);
                }
                Ok::<_, String>((
                    book.name,
                    Arc::new(doc),
                    page_count,
                    first,
                    book_dir,
                    prefix,
                    start_page,
                ))
            })
            .await;
            match opened {
                Ok(Ok((
                    book_name,
                    _doc,
                    page_count,
                    first,
                    book_dir,
                    page_ratio_prefix,
                    start_page,
                ))) => {
                    let _ = desktop.set_title(&format!("UeberNeon — {book_name}"));
                    let mut cache = HashMap::new();
                    let first_warning = first.warning.clone();
                    cache.insert(start_page, first);
                    session.set(Some(ReaderSession {
                        doc: _doc.clone(),
                        page_count,
                        cache,
                        page_ratio_prefix,
                        render_target: Some(start_page),
                        render_epoch: 0,
                        jump_lock_until: std::time::Instant::now(),
                        book_id: parse_id.clone(),
                        book_name,
                        book_dir: book_dir.clone(),
                        selection: None,
                        drag_anchor: None,
                        dragging: false,
                        shift_extending: false,
                        ocr_cache: SingleSlotCache::new(),
                        pending_copy: None,
                        copy_busy: false,
                        action_bar: None,
                        action_bar_gen: 0,
                        translation: None,
                        translation_gen: 0,
                        ocr_warning_shown: false,
                    }));
                    current_page.set(start_page);
                    page_input.set(start_page.to_string());
                    if let Some(cit) = initial_citation {
                        open_citation(
                            session,
                            desktop,
                            cit.page,
                            cit.quote,
                            current_page,
                            page_input,
                            page_loading,
                            zoom,
                        );
                    } else {
                        request_jump(session, desktop, start_page, page_loading, zoom);
                    }
                    if let Some(warn) = first_warning {
                        if let Some(inner) = session.write().as_mut() {
                            inner.ocr_warning_shown = true;
                        }
                        error_signal.write().push(ErrorInfo::new(
                            "reader-ocr-unavailable",
                            "页面 OCR 不可用",
                            warn,
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                    // 启动唯一渲染 worker：首页已就绪，直接进入预取相邻页。
                    spawn_render_worker(session, error_signal, page_loading);

                    // 打开书时补跑后台 OCR(未配置模型时静默跳过)。
                    crate::page_ocr::manager().ensure_started(&parse_id);

                    // 目录后台加载：书签优先，无书签时字号识别（可能较慢）。
                    {
                        let toc = toc;
                        let toc_collapsed = toc_collapsed;
                        let bd = book_dir.clone();
                        spawn(async move {
                            let mut toc = toc;
                            let mut toc_collapsed = toc_collapsed;
                            let result = tokio::task::spawn_blocking(move || {
                                crate::pdf::toc::load_or_generate(&bd, false)
                            })
                            .await;
                            if let Ok(t) = result {
                                toc_collapsed.set(crate::pdf::toc::default_collapsed(&t.items));
                                toc.set(Some(t));
                            }
                        });
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

    let on_scroll = {
        let current_page = current_page;
        let page_input = page_input;
        let session = session;
        let last_scroll_width = last_scroll_width;
        move |evt: ScrollEvent| {
            let top = evt.scroll_top();
            let ch = evt.client_height() as f64;
            let cw = evt.client_width();
            if (*client_width.read() - cw as f64).abs() > 0.5 {
                client_width.set(cw as f64);
            }
            update_current_page(
                session,
                current_page,
                page_input,
                last_scroll_width,
                top,
                cw,
                ch as i32,
                zoom(),
            );
            schedule_render(session, *current_page.read(), page_loading);
            // 有翻译卡片时，滚动不关闭操作栏和翻译块（选区与浮层保持可见）。
            let has_translation = session
                .read()
                .as_ref()
                .and_then(|s| s.translation.as_ref())
                .is_some();
            if !has_translation {
                close_action_bar(session);
            }
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

    // 「本页 OCR」:强制重跑当前页(处理扫描页 / 有字形但提取损坏的页面),
    // 完成后清缓存并提升渲染 epoch,让唯一渲染 worker 用 OCR 词层重绘。
    let on_page_ocr = {
        let session = session;
        let page_loading = page_loading;
        let error_signal = error_signal;
        move |_| {
            let Some((book_dir, doc)) = session
                .read()
                .as_ref()
                .map(|s| (s.book_dir.clone(), s.doc.clone()))
            else {
                return;
            };
            let page = *current_page.read();
            spawn(async move {
                let mut session = session;
                let mut page_loading = page_loading;
                let mut error_signal = error_signal;
                let result = tokio::task::spawn_blocking(move || {
                    crate::page_ocr::reocr_page(&book_dir, page, &doc)
                })
                .await;
                match &result {
                    Ok(Ok(_)) => {
                        if let Some(inner) = session.write().as_mut() {
                            inner.cache.remove(&page);
                            inner.render_epoch += 1;
                            inner.render_target = Some(page);
                        }
                        page_loading.set(Some(page));
                        if let Ok(Ok(None)) = &result {
                            error_signal.write().push(ErrorInfo::new(
                                "reader-ocr-unavailable",
                                "未配置页面 OCR 模型",
                                "请把本地 ONNX 模型包放到 ~/.ueberneon/page-ocr-models/ 并在设置中启用",
                                ErrorSeverity::Warning,
                                ErrorSource::General,
                            ));
                        }
                    }
                    Ok(Err(e)) => {
                        error_signal.write().push(ErrorInfo::new(
                            "reader-ocr-failed",
                            "页面 OCR 失败",
                            e.to_string(),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                    Err(e) => {
                        error_signal.write().push(ErrorInfo::new(
                            "reader-ocr-failed",
                            "页面 OCR 失败",
                            format!("{e}"),
                            ErrorSeverity::Warning,
                            ErrorSource::General,
                        ));
                    }
                }
            });
        }
    };

    let (book_name, page_count, zoom_now, selection, copy_busy, action_bar, translation) = session
        .read()
        .as_ref()
        .map(|s| {
            (
                s.book_name.clone(),
                s.page_count,
                zoom(),
                s.selection.clone(),
                s.copy_busy,
                s.action_bar,
                s.translation.clone(),
            )
        })
        .unwrap_or_else(|| (String::new(), 0u32, 100u32, None, false, None, None));

    let toc_data = toc.read().clone();
    let toc_source_label = toc_data
        .as_ref()
        .map(|t| t.source.label().to_string())
        .unwrap_or_default();
    let toc_items: Vec<crate::pdf::toc::TocItem> = toc_data
        .as_ref()
        .map(|t| t.items.clone())
        .unwrap_or_default();
    let toc_collapsed_set = toc_collapsed.read().clone();
    let toc_visible: Vec<(crate::pdf::toc::TocItem, bool)> =
        crate::pdf::toc::visible_toc_items(&toc_items, &toc_collapsed_set);
    let current_page_now = *current_page.read();
    let active_toc_id = {
        let explicit = toc_active_id.read().clone();
        let computed = || -> Option<String> {
            toc_visible
                .iter()
                .filter(|(it, _)| it.page <= current_page_now)
                .max_by_key(|(it, _)| (it.page, it.level))
                .map(|(it, _)| it.id.clone())
        };
        match explicit {
            Some((id, page))
                if page == current_page_now && toc_visible.iter().any(|(it, _)| it.id == id) =>
            {
                Some(id)
            }
            _ => computed(),
        }
    };
    let toc_open_now = *toc_open.read();
    let page_loading_now = *page_loading.read();
    // 页面虚拟化：按当前页只渲染窗口内的页，前后用分隔块撑起滚动高度。
    let page_w = {
        let cw = *client_width.read();
        ((cw - 48.0).max(200.0)) * zoom_now as f64 / 100.0
    };
    let (window_start, window_end) = window_bounds(current_page_now, page_count, PAGE_WINDOW);
    let (top_spacer, bottom_spacer) = session
        .read()
        .as_ref()
        .map(|inner| {
            spacer_heights(
                &inner.page_ratio_prefix,
                page_w,
                28.0,
                window_start,
                window_end,
            )
        })
        .unwrap_or((0.0, 0.0));

    let on_toggle_toc = move |_| {
        let mut t = toc_open;
        let cur = *t.read();
        t.set(!cur);
    };
    let on_toggle_toc_item = {
        let mut collapsed = toc_collapsed;
        move |id: String| {
            let mut set = collapsed.read().clone();
            if !set.insert(id.clone()) {
                set.remove(&id);
            }
            collapsed.set(set);
        }
    };
    let on_rebuild_toc = {
        let mut toc = toc;
        let toc_collapsed = toc_collapsed;
        let session = session;
        move |_| {
            let bd = session.read().as_ref().map(|s| s.book_dir.clone());
            if let Some(bd) = bd {
                toc.set(None);
                let toc = toc;
                spawn(async move {
                    let mut toc = toc;
                    let mut toc_collapsed = toc_collapsed;
                    let r = tokio::task::spawn_blocking(move || {
                        crate::pdf::toc::load_or_generate(&bd, true)
                    })
                    .await;
                    if let Ok(t) = r {
                        toc_collapsed.set(crate::pdf::toc::default_collapsed(&t.items));
                        toc.set(Some(t));
                    }
                });
            }
        }
    };
    let on_page_input = move |evt: FormEvent| {
        let mut page_input = page_input;
        page_input.set(evt.value());
    };
    let on_page_keydown = {
        let desktop = desktop.clone();
        let session = session;
        let page_loading = page_loading;
        let zoom = zoom;
        let mut current_page = current_page;
        let mut page_input = page_input;
        move |evt: KeyboardEvent| {
            if evt.key().to_string() != "Enter" {
                return;
            }
            let raw = page_input.read().clone();
            if let Some(p) = crate::pdf::toc::clamp_page(&raw, page_count) {
                current_page.set(p);
                page_input.set(p.to_string());
                request_jump(session, desktop.clone(), p, page_loading, zoom);
            }
        }
    };

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
            bar.explain_enabled,
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
        (
            tc.x,
            tc.y,
            tc.status,
            tc.text.clone(),
            status_class,
            tc.source_sentences.clone(),
            tc.translated_sentences.clone(),
            tc.groups.clone(),
        )
    });

    let origin_name = project_id.as_ref().and_then(|pid| {
        let name = crate::db::with_db(|conn| {
            crate::db::metadata::project::get(conn, pid)
                .ok()
                .flatten()
                .map(|r| r.name)
                .unwrap_or_default()
        });
        (!name.is_empty()).then_some(name)
    });
    let chat_options: Vec<DropdownOption> = chat_conversations
        .read()
        .iter()
        .map(|c| DropdownOption {
            value: c.id.clone(),
            label: if c.title.trim().is_empty() {
                "new conversation".to_string()
            } else {
                c.title.clone()
            },
        })
        .collect();

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
                    "aria-label": "目录",
                    onclick: on_toggle_toc,
                    "☰"
                }
                span { class: "reader-title", "{book_name}" }
                if let Some(ref origin) = origin_name {
                    span { class: "reader-origin", "计划 · {origin}" }
                }
                div {
                    class: "reader-page-control",
                    input {
                        class: "reader-page-input",
                        value: page_input,
                        oninput: on_page_input,
                        onkeydown: on_page_keydown,
                        "aria-label": "页码",
                    }
                    span { class: "reader-page-total", "/ {page_count}" }
                }
                div {
                    class: "reader-toolbar",
                    button {
                        class: "btn btn-cancel reader-chat-toggle",
                        onclick: {
                            let mut chat_open = chat_open;
                            move |_| {
                                let open = *chat_open.read();
                                chat_open.set(!open);
                            }
                        },
                        if *chat_open.read() { "隐藏对话" } else { "对话" }
                    }
                    button {
                        class: "btn btn-cancel reader-search-toggle",
                        onclick: {
                            let mut search_open = search_open;
                            move |_| {
                                let open = *search_open.read();
                                search_open.set(!open);
                            }
                        },
                        if *search_open.read() { "关闭搜索" } else { "搜索" }
                    }
                    if copy_busy {
                        span { class: "reader-copy-status", "识别公式中…" }
                    }
                    button {
                        class: "btn btn-cancel",
                        onclick: on_page_ocr,
                        "本页 OCR"
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
                    button {
                        class: "btn btn-cancel reader-close",
                        onclick: move |_| on_back.call(()),
                        "← 关闭"
                    }
                }
            }
            if let Some(n) = page_loading_now {
                div {
                    class: "reader-loading-badge",
                    span { class: "reader-spinner reader-spinner--sm" }
                    "正在加载第 {n} 页…"
                }
            }
            if session.read().is_none() {
                div {
                    class: "reader-loading",
                    span { class: "reader-spinner" }
                    span { class: "reader-loading__label", "打开 PDF 中…" }
                }
            } else {
                div {
                    class: if *chat_open.read() {
                        "reader-body has-chat"
                    } else {
                        "reader-body"
                    },
                    if *search_open.read() {
                        div {
                            class: "reader-search",
                            div {
                                class: "reader-search-head",
                                span { class: "reader-search-title", "全书搜索" }
                                button {
                                    class: "reader-search-close",
                                    onclick: {
                                        let mut search_open = search_open;
                                        move |_| search_open.set(false)
                                    },
                                    "✕"
                                }
                            }
                            div {
                                class: "reader-search-input-row",
                                input {
                                    class: "reader-search-input",
                                    value: search_query,
                                    placeholder: "搜索关键词…",
                                    oninput: {
                                        let mut search_query = search_query;
                                        move |evt: FormEvent| search_query.set(evt.value())
                                    },
                                    onkeydown: {
                                        let session = session;
                                        let search_query = search_query;
                                        let search_results = search_results;
                                        let search_running = search_running;
                                        let search_gen = search_gen;
                                        move |evt: KeyboardEvent| {
                                            if evt.key().to_string() == "Enter" {
                                                run_book_search(
                                                    session,
                                                    search_query,
                                                    search_results,
                                                    search_running,
                                                    search_gen,
                                                );
                                            }
                                        }
                                    },
                                }
                                button {
                                    class: "btn btn-send",
                                    disabled: *search_running.read(),
                                    onclick: {
                                        let session = session;
                                        let search_query = search_query;
                                        let search_results = search_results;
                                        let search_running = search_running;
                                        let search_gen = search_gen;
                                        move |_| {
                                            run_book_search(
                                                session,
                                                search_query,
                                                search_results,
                                                search_running,
                                                search_gen,
                                            )
                                        }
                                    },
                                    if *search_running.read() { "…" } else { "搜索" }
                                }
                            }
                            if *search_running.read() {
                                div { class: "reader-search-status", "搜索中…" }
                            } else if search_results.read().is_empty()
                                && !search_query.read().trim().is_empty()
                            {
                                div { class: "reader-search-empty", "无匹配结果" }
                            } else {
                                div {
                                    class: "reader-search-results",
                                    for (page, snippet) in search_results.read().iter() {
                                        {
                                            let p = *page;
                                            let s = snippet.clone();
                                            rsx! {
                                                button {
                                                    class: "reader-search-result",
                                                    onclick: {
                                                        let session = session;
                                                        let desktop = desktop.clone();
                                                        let current_page = current_page;
                                                        let page_input = page_input;
                                                        let page_loading = page_loading;
                                                        let zoom = zoom;
                                                        let s2 = s.clone();
                                                        move |_| {
                                                            open_citation(
                                                                session,
                                                                desktop.clone(),
                                                                p,
                                                                s2.clone(),
                                                                current_page,
                                                                page_input,
                                                                page_loading,
                                                                zoom,
                                                            )
                                                        }
                                                    },
                                                    span {
                                                        class: "reader-search-result-page",
                                                        "P{p}"
                                                    }
                                                    span {
                                                        class: "reader-search-result-snippet",
                                                        "{s}"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if toc_open_now {
                        div {
                            class: "reader-toc",
                            div {
                                class: "reader-toc-head",
                                span { class: "reader-toc-title", "目录" }
                                if !toc_source_label.is_empty() {
                                    span { class: "reader-toc-badge", "{toc_source_label}" }
                                }
                                button {
                                    class: "btn btn-cancel",
                                    onclick: on_rebuild_toc,
                                    "重建"
                                }
                            }
                            if toc_items.is_empty() {
                                div {
                                    class: if toc_data.is_none() {
                                        "reader-toc-empty is-loading"
                                    } else {
                                        "reader-toc-empty"
                                    },
                                    if toc_data.is_none() {
                                        span { class: "reader-spinner reader-spinner--sm" }
                                        "目录生成中…"
                                    } else {
                                        "这本书还没有目录"
                                    }
                                }
                            } else {
                                div {
                                    class: "reader-toc-list",
                                    for (item, is_parent) in &toc_visible {
                                        div {
                                            class: {
                                                let active = active_toc_id.as_deref() == Some(item.id.as_str());
                                                let mut c: String = if active {
                                                    "reader-toc-item is-active".to_string()
                                                } else {
                                                    "reader-toc-item".to_string()
                                                };
                                                if *is_parent {
                                                    c.push_str(" is-parent");
                                                }
                                                c
                                            },
                                            style: "padding-left: {12 + item.level * 14}px",
                                            onclick: {
                                                let p = item.page;
                                                let id = item.id.clone();
                                                let desktop = desktop.clone();
                                                let session = session;
                                                let page_loading = page_loading;
                                                let zoom = zoom;
                                                let mut current_page = current_page;
                                                let mut page_input = page_input;
                                                let mut toc_active_id = toc_active_id;
                                                move |_| {
                                                    current_page.set(p);
                                                    page_input.set(p.to_string());
                                                    toc_active_id.set(Some((id.clone(), p)));
                                                    request_jump(
                                                        session,
                                                        desktop.clone(),
                                                        p,
                                                        page_loading,
                                                        zoom,
                                                    );
                                                }
                                            },
                                            if *is_parent {
                                                span {
                                                    class: "reader-toc-item__chevron",
                                                    "aria-label": "收起/展开",
                                                    onclick: {
                                                        let id = item.id.clone();
                                                        let mut on_toggle = on_toggle_toc_item;
                                                        move |evt: MouseEvent| {
                                                            evt.stop_propagation();
                                                            on_toggle(id.clone());
                                                        }
                                                    },
                                                    if toc_collapsed_set.contains(&item.id) {
                                                        "▸"
                                                    } else {
                                                        "▾"
                                                    }
                                                }
                                            }
                                            span { class: "reader-toc-item__title", "{item.title}" }
                                            span { class: "reader-toc-item__page", "{item.page}" }
                                        }
                                    }
                                }
                            }
                        }
                    } else {
                        div {
                            class: "reader-toc-rail",
                            button {
                                class: "btn btn-cancel",
                                onclick: on_toggle_toc,
                                "☰"
                            }
                        }
                    }
                    div {
                        class: "reader-scroll",
                        tabindex: "-1",
                        onscroll: on_scroll,
                        onmounted: move |evt: MountedEvent| {
                            let mut client_width = client_width;
                            spawn(async move {
                                if let Ok(rect) = evt.data.get_client_rect().await {
                                    let cw = rect.width();
                                    if (*client_width.read() - cw).abs() > 0.5 {
                                        client_width.set(cw);
                                    }
                                }
                            });
                        },
                        onresize: move |evt: ResizeEvent| {
                            if let Ok(size) = evt.data.get_content_box_size() {
                                // 内容盒宽度 + 左右 padding(24px×2) = clientWidth。
                                let cw = size.width + 48.0;
                                if (*client_width.read() - cw).abs() > 0.5 {
                                    client_width.set(cw);
                                }
                            }
                        },
                        onmouseleave: on_scroll_mouseleave,
                        onkeydown: on_keydown,
                        oncontextmenu: move |evt| {
                            evt.prevent_default();
                            let coords = evt.client_coordinates();
                            open_action_bar(session, coords.x, coords.y);
                        },
                        if window_start > 1 {
                            div {
                                class: "reader-page-spacer",
                                style: "height: {top_spacer}px",
                                "data-spacer": "top",
                            }
                        }
                        for page in window_start..=window_end {
                            {
                                let page = page;
                                let session = session;
                                let click_state = click_state;
                                let desktop = desktop.clone();
                                let selection = selection.as_ref();
                                rsx! {
                                    {
                                        let guard = session.read();
                                    let placeholder_aspect = guard
                                        .as_ref()
                                        .map(|s| page_aspect_at(&s.page_ratio_prefix, page))
                                        .unwrap_or(1.0);
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
                                                    "data-page": "{page}",
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
                                        None => {
                                            rsx! {
                                                div {
                                                    class: "reader-page-placeholder",
                                                    style: "width: {zoom_now}%; aspect-ratio: {placeholder_aspect:.6}",
                                                    "data-page": "{page}",
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        }
                        if window_end < page_count {
                            div {
                                class: "reader-page-spacer",
                                style: "height: {bottom_spacer}px",
                                "data-spacer": "bottom",
                            }
                        }
                }
                }
            }
            if let Some((
                bar,
                status_class,
                label,
                show_formula,
                translation_enabled,
                explain_enabled,
                translation_loading,
            )) = action_bar_view
            {
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
                            onclick: {
                                let desktop = desktop.clone();
                                move |_| {
                                    let vw =
                                        desktop.inner_size().width as f64 / desktop.scale_factor();
                                    let vh =
                                        desktop.inner_size().height as f64 / desktop.scale_factor();
                                    action_bar_translate(session, (vw, vh));
                                }
                            },
                            "翻译"
                        }
                    }
                    if explain_enabled {
                        button {
                            class: "reader-actionbar__btn",
                            onclick: move |_| {
                                action_bar_explain(session, on_explain_prompt.clone())
                            },
                            "解释"
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
            if let Some((
                x,
                y,
                status,
                text,
                status_class,
                source_sentences,
                translated_sentences,
                groups,
            )) = translation_view
            {
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
                        div {
                            class: "reader-translation-card__split",
                            onmouseleave: {
                                let mut translation_hover = translation_hover;
                                move |_| translation_hover.set(None)
                            },
                            div {
                                class: "reader-translation-card__col",
                                for (i, s) in source_sentences.iter().enumerate() {
                                    {
                                        let i = i;
                                        let s = s.clone();
                                        let hovered = match *translation_hover.read() {
                                            Some((0, k)) => k == i,
                                            Some((1, j)) => crate::translate::translation_source_index(
                                                &groups,
                                                j,
                                            ) == Some(i),
                                            _ => false,
                                        };
                                        rsx! {
                                            div {
                                                class: if hovered {
                                                    "reader-translation-card__sentence is-highlight"
                                                } else {
                                                    "reader-translation-card__sentence"
                                                },
                                                onmouseenter: {
                                                    let mut translation_hover = translation_hover;
                                                    move |_| translation_hover.set(Some((0, i)))
                                                },
                                                "{s}"
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "reader-translation-card__divider", "aria-hidden": "true" }
                            div {
                                class: "reader-translation-card__col",
                                for (j, t) in translated_sentences.iter().enumerate() {
                                    {
                                        let j = j;
                                        let t = t.clone();
                                        let hovered = match *translation_hover.read() {
                                            Some((1, k)) => k == j,
                                            Some((0, i)) => groups
                                                .get(i)
                                                .map(|&(s, e)| j >= s && j <= e)
                                                .unwrap_or(false),
                                            _ => false,
                                        };
                                        rsx! {
                                            div {
                                                class: if hovered {
                                                    "reader-translation-card__sentence is-highlight"
                                                } else {
                                                    "reader-translation-card__sentence"
                                                },
                                                onmouseenter: {
                                                    let mut translation_hover = translation_hover;
                                                    move |_| translation_hover.set(Some((1, j)))
                                                },
                                                "{t}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    } else if status == ActionBarStatus::Error {
                        div { class: "reader-translation-card__error", "{text}" }
                    }
                    div {
                        class: "reader-translation-card__actions",
                        button {
                            class: "reader-actionbar__btn",
                            onclick: move |_| retry_translation(session),
                            "重试"
                        }
                    }
                }
            }
            if *chat_open.read() {
                div {
                    class: "reader-chat-side",
                    div {
                        class: "reader-chat-side__head",
                        span { class: "reader-chat-side__title", "{book_name} · 对话" }
                        div {
                            class: "reader-chat-side__select",
                            Dropdown {
                                value: chat_active_conv(),
                                onchange: on_select_chat_conversation,
                                options: chat_options,
                                placeholder: "选择对话",
                                searchable: Some(true),
                            }
                        }
                        button {
                            class: "reader-chat-side__new",
                            title: "新建对话",
                            onclick: {
                                let on_new = on_new_chat_conversation;
                                move |_| on_new.call(())
                            },
                            "+"
                        }
                        button {
                            class: "reader-chat-side__delete",
                            title: "删除对话",
                            onclick: {
                                let on_delete = on_delete_chat_conversation;
                                move |_| on_delete.call(())
                            },
                            "删除"
                        }
                        button {
                            class: "reader-chat-side__close",
                            onclick: move |_| {
                                let mut chat_open = chat_open;
                                chat_open.set(false);
                            },
                            "✕"
                        }
                    }
                    ChatPanel {
                        runtimes: chat_runtimes,
                        active_conv_id: chat_active_conv,
                        is_streaming: chat_streaming,
                        markdown_to_html: reader_markdown_to_html,
                        on_approve: {
                            let atx = chat_approval_tx;
                            let cid = chat_active_conv;
                            move |(tool_call_id, allowed): (String, bool)| {
                                let conv_id = cid();
                                if let Some(tx) = atx.read().get(&conv_id).cloned() {
                                    let _ = tx.try_send((tool_call_id, allowed));
                                }
                            }
                        },
                        citation_handler: {
                            let book_id_handler = book_id_for_handler.clone();
                            let project_id_handler = project_id_for_handler.clone();
                            Some(Callback::new(move |c: BookCitation| {
                                if c.book_id == book_id_handler {
                                    let mut citation_target = citation_target;
                                    citation_target.set(Some((c.page, c.quote)));
                                } else {
                                    crate::ui::reader_window::open_with_project_and_citation(
                                        c.book_id.clone(),
                                        project_id_handler.clone(),
                                        Some(c),
                                    );
                                }
                            }))
                        },
                    }
                    InputBar {
                        is_streaming: chat_streaming,
                        action_mode: chat_action_mode,
                        agent_mode: chat_agent_mode,
                        agent_configs: chat_agent_config(),
                        selected_agent_config_id: chat_agent_config_id(),
                        on_agent_config_change: move |_new_id: String| {},
                        on_agent_mode_change: {
                            let mut am = chat_agent_mode;
                            move |mode: AgentMode| am.set(mode)
                        },
                        config_disabled: true,
                        approval_hint_text: chat_approval_hint,
                        on_send: send_chat_message,
                        on_cancel: {
                            let rt_sig = chat_runtimes;
                            let cid_sig = chat_active_conv;
                            move |_| {
                                let cid = cid_sig();
                                if let Some(rt) = rt_sig.read().get(&cid)
                                    && let Some(ref token) = rt.cancel_token
                                {
                                    token.cancel();
                                }
                            }
                        },
                    }
                }
            }
        }
    }
}

// ── 全书搜索：pages/*.md 全文检索，返回 (页码, 片段) ──────────────────────

fn search_book_pages(
    dir: &Path,
    query: &str,
    max_results: usize,
    max_chars: usize,
) -> Vec<(u32, String)> {
    let pages_dir = crate::layout::book_pages_dir(dir);
    let mut entries: Vec<_> = std::fs::read_dir(&pages_dir)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .collect();
    entries.sort_by_key(|e| e.file_name());
    let q = query.to_lowercase();
    let mut out = Vec::new();
    let mut total = 0usize;
    'pages: for entry in entries {
        if out.len() >= max_results || total >= max_chars {
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
            let snippet = line.trim().to_string();
            total += snippet.chars().count() + 1;
            out.push((page_no, snippet));
            if out.len() >= max_results || total >= max_chars {
                break 'pages;
            }
        }
    }
    out
}

/// 触发一次全书搜索（阻塞读盘放 spawn_blocking，结果按代数丢弃过期返回）。
fn run_book_search(
    session: Signal<Option<ReaderSession>>,
    query: Signal<String>,
    mut results: Signal<Vec<(u32, String)>>,
    mut running: Signal<bool>,
    mut search_gen: Signal<u64>,
) {
    let q = query.read().trim().to_string();
    if q.is_empty() {
        results.set(Vec::new());
        return;
    }
    let Some(dir) = session.read().as_ref().map(|s| s.book_dir.clone()) else {
        return;
    };
    running.set(true);
    let g = search_gen() + 1;
    search_gen.set(g);
    spawn(async move {
        let q2 = q.clone();
        let dir2 = dir.clone();
        let out = tokio::task::spawn_blocking(move || {
            search_book_pages(&dir2, &q2, 50, 12_000)
        })
        .await
        .unwrap_or_default();
        if search_gen() != g {
            return;
        }
        results.set(out);
        running.set(false);
    });
}

// ── 引用指针：跳页 + 原文高亮 ───────────────────────────────────────────────

/// 归一化文本：去掉空白并转小写，便于模糊匹配引用片段。
fn normalize_quote(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// 在某一页正文/小字层中查找 quote，构造高亮选区。
fn find_quote_selection(inner: &ReaderSession, page: u32, quote: &str) -> Option<Selection> {
    let q = normalize_quote(quote);
    if q.is_empty() {
        return None;
    }
    let rendered = inner.cache.get(&page)?;
    for layer in [Layer::Body, Layer::Small] {
        let flat = layer_flat(rendered, layer);
        if flat.is_empty() {
            continue;
        }
        let mut hay = String::new();
        let mut char_to_word: Vec<usize> = Vec::new();
        for (i, w) in flat.iter().enumerate() {
            let start = hay.chars().count();
            hay.push_str(&normalize_quote(&w.text));
            for _ in start..hay.chars().count() {
                char_to_word.push(i);
            }
        }
        let Some(pos) = hay.find(&q) else {
            continue;
        };
        let end = pos + q.chars().count();
        if pos >= char_to_word.len() || end.saturating_sub(1) >= char_to_word.len() {
            continue;
        }
        let lo = char_to_word[pos];
        let hi = char_to_word[end - 1];
        return Some(Selection {
            layer,
            steps: vec![SelectionStep {
                page,
                lo,
                hi,
                column_left: None,
            }],
            formula: false,
            formula_score: 0.0,
            anchor: None,
        });
    }
    None
}

/// 点击引用 chip：跳转到目标页并等待渲染完成后高亮原文。
fn open_citation(
    mut session: Signal<Option<ReaderSession>>,
    desktop: dioxus::desktop::DesktopContext,
    page: u32,
    quote: String,
    mut current_page: Signal<u32>,
    mut page_input: Signal<String>,
    page_loading: Signal<Option<u32>>,
    zoom: Signal<u32>,
) {
    current_page.set(page);
    page_input.set(page.to_string());
    request_jump(session, desktop, page, page_loading, zoom);
    spawn(async move {
        for _ in 0..50 {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let ready = session
                .read()
                .as_ref()
                .map(|s| s.cache.contains_key(&page))
                .unwrap_or(false);
            if ready {
                break;
            }
        }
        let mut guard = session.write();
        if let Some(inner) = guard.as_mut() {
            if let Some(sel) = find_quote_selection(inner, page, &quote) {
                inner.selection = Some(sel);
                inner.action_bar = None;
                inner.translation = None;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_book_pages_returns_sorted_page_snippets() {
        let dir = std::env::temp_dir().join(format!(
            "ueberneon-reader-search-test-{}",
            std::process::id()
        ));
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

        let out = search_book_pages(&dir, "代数", 50, 6000);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].0, 1);
        assert!(out[0].1.contains("线性代数定义"));
        assert_eq!(out[1].0, 2);

        let _ = std::fs::remove_dir_all(&dir);
    }

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
            font_size_pt: 10.0,
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

    fn overlay_line_with_font(
        words: Vec<(String, f64, f64, f64, f64)>,
        font_size_pt: f64,
    ) -> OverlayLine {
        let mut line = overlay_line(words);
        line.font_size_pt = font_size_pt;
        line
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
            fw(1, "combined", 10.0, 2.5, 2.0),
            fw(1, " ", 20.0, 2.5, 2.0),
            fw(1, "with", 21.0, 2.5, 2.0),
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
                anchor: None,
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
                anchor: None,
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
            fw(1, "combined", 10.0, 2.5, 2.0),
            fw(1, "with", 20.0, 2.5, 2.0),
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
                anchor: None,
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
    fn sentence_walk_stops_at_heading_above_paragraph() {
        // 标题行没有句读,但与正文之间有行距 → 三击正文首句不应包含标题
        let flat = vec![
            fw(0, "Hypothesis", 8.0, 0.0, 2.0),
            fw(0, " ", 20.0, 0.0, 2.0),
            fw(0, "4:", 21.0, 0.0, 2.0),
            fw(0, " ", 24.0, 0.0, 2.0),
            fw(0, "The", 25.0, 0.0, 2.0),
            fw(0, " ", 29.0, 0.0, 2.0),
            fw(0, "Agent-Driven", 30.0, 0.0, 2.0),
            fw(0, " ", 43.0, 0.0, 2.0),
            fw(0, "Economy", 44.0, 0.0, 2.0),
            fw(1, "The", 8.0, 6.0, 2.0),
            fw(1, " ", 12.0, 6.0, 2.0),
            fw(1, "fourth", 13.0, 6.0, 2.0),
            fw(1, " ", 20.0, 6.0, 2.0),
            fw(1, "hypothesis", 21.0, 6.0, 2.0),
            fw(2, "is", 8.0, 8.5, 2.0),
            fw(2, " ", 11.0, 8.5, 2.0),
            fw(2, "that", 12.0, 8.5, 2.0),
            fw(3, "models.", 8.0, 11.0, 2.0),
        ];
        let steps = sentence_walk(&flat, 9, None, None, 0);
        assert_eq!(steps.len(), 1, "句子应止于本段,不跨回标题");
        let copied = copy_steps(
            &Selection {
                layer: Layer::Body,
                formula: false,
                formula_score: 0.0,
                anchor: None,
                steps,
            },
            |_| Some(&flat),
        )
        .unwrap();
        assert!(!copied.contains("Hypothesis"), "{copied}");
        assert!(copied.starts_with("The fourth hypothesis"), "{copied}");
        assert!(copied.ends_with("models."), "{copied}");
    }

    #[test]
    fn sentence_walk_stops_at_paragraph_end_without_punctuation() {
        // 段内没有句读且段落到行尾结束 → 句子止于段尾,不吞下一段
        let flat = vec![
            fw(0, "First", 8.0, 0.0, 2.0),
            fw(1, "line", 8.0, 2.5, 2.0),
            fw(2, "Next", 8.0, 8.0, 2.0),
        ];
        let steps = sentence_walk(&flat, 1, None, None, 0);
        let copied = copy_steps(
            &Selection {
                layer: Layer::Body,
                formula: false,
                formula_score: 0.0,
                anchor: None,
                steps,
            },
            |_| Some(&flat),
        )
        .unwrap();
        assert_eq!(copied, "First line", "{copied}");
    }

    #[test]
    fn copy_text_joins_wrapped_lines_and_keeps_paragraph_breaks() {
        let flat = vec![
            fw(0, "I am", 0.0, 0.0, 2.0),
            fw(0, " ", 5.0, 0.0, 2.0),
            fw(0, "also", 6.0, 0.0, 2.0),
            fw(1, "indebted", 0.0, 2.5, 2.0),
            fw(1, " ", 9.0, 2.5, 2.0),
            fw(1, "to", 10.0, 2.5, 2.0),
            fw(2, "Next", 0.0, 8.0, 2.0),
        ];
        // 软换行合并为空格；垂直间隙大的段落分界保留换行
        assert_eq!(
            copy_text_filtered(&flat, 0, 6, None),
            "I am also indebted to\nNext"
        );
    }

    #[test]
    fn copy_text_joins_cjk_wrapped_lines_without_space() {
        let flat = vec![fw(0, "你好", 0.0, 0.0, 2.0), fw(1, "世界", 0.0, 2.5, 2.0)];
        assert_eq!(copy_text_filtered(&flat, 0, 1, None), "你好世界");
    }

    #[test]
    fn translation_input_joins_wrapped_lines_with_space() {
        let flat = vec![fw(0, "As", 0.0, 0.0, 2.0), fw(1, "models", 0.0, 2.5, 2.0)];
        let sel = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            anchor: None,
            steps: vec![SelectionStep {
                page: 1,
                lo: 0,
                hi: 1,
                column_left: None,
            }],
        };
        let (text, formulas) = selection_translation_input(&sel, |_| Some(&flat)).unwrap();
        assert_eq!(text, "As models");
        assert!(formulas.is_empty());
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
        // 判定依据是真实字号:脚注字形盒高度与正文相同(2.0),但字号 8pt
        // (正文 10pt),仍应进小字层;窄碎片 "a" 同样按小字处理。
        let overlay = vec![
            overlay_line(vec![("Hello".into(), 10.0, 0.0, 20.0, 2.0)]),
            overlay_line(vec![("World".into(), 10.0, 5.0, 20.0, 2.0)]),
            overlay_line(vec![("More".into(), 10.0, 10.0, 20.0, 2.0)]),
            overlay_line_with_font(vec![("footnote".into(), 10.0, 10.0, 30.0, 2.0)], 8.0),
            overlay_line_with_font(vec![("a".into(), 3.0, 0.0, 0.5, 1.0)], 8.0),
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
            anchor: None,
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

    fn page_with_words(words: &[&str]) -> RenderedPage {
        RenderedPage {
            src: String::new(),
            body: words
                .iter()
                .enumerate()
                .map(|(i, w)| fw(0, w, i as f64, 0.0, 2.0))
                .collect(),
            small: Vec::new(),
            w_pt: 1.0,
            h_pt: 1.0,
            warning: None,
        }
    }

    fn selection_with_anchor(page: u32, layer: Layer, idx: usize) -> Selection {
        Selection {
            layer,
            formula: false,
            formula_score: 0.0,
            anchor: Some((page, layer, idx)),
            steps: vec![SelectionStep {
                page,
                lo: idx,
                hi: idx,
                column_left: None,
            }],
        }
    }

    #[test]
    fn shift_extend_same_page_forward_shrink_and_self() {
        let mut cache = HashMap::new();
        cache.insert(1, page_with_words(&["a", "b", "c", "d", "e"]));
        let sel = selection_with_anchor(1, Layer::Body, 1);

        let ext = shift_extend_selection(&cache, &sel, 1, Layer::Body, 3).unwrap();
        assert_eq!(ext.steps.len(), 1);
        assert_eq!(
            (ext.steps[0].page, ext.steps[0].lo, ext.steps[0].hi),
            (1, 1, 3)
        );
        assert_eq!(ext.anchor, Some((1, Layer::Body, 1)));

        let ext = shift_extend_selection(&cache, &sel, 1, Layer::Body, 0).unwrap();
        assert_eq!((ext.steps[0].lo, ext.steps[0].hi), (0, 1));

        let ext = shift_extend_selection(&cache, &sel, 1, Layer::Body, 1).unwrap();
        assert_eq!((ext.steps[0].lo, ext.steps[0].hi), (1, 1));
    }

    #[test]
    fn shift_extend_cross_page_forward_and_backward_order() {
        let mut cache = HashMap::new();
        cache.insert(1, page_with_words(&["a", " ", "b", " ", "c"]));
        cache.insert(2, page_with_words(&["d", " ", "e", " ", "f", " ", "g"]));

        // 前向:锚点页 1 的 "b"(idx2) → 页 2 的 "f"(idx4)。
        let sel = selection_with_anchor(1, Layer::Body, 2);
        let ext = shift_extend_selection(&cache, &sel, 2, Layer::Body, 4).unwrap();
        assert_eq!(ext.steps.len(), 2);
        assert_eq!(
            (ext.steps[0].page, ext.steps[0].lo, ext.steps[0].hi),
            (1, 2, 4)
        );
        assert_eq!(
            (ext.steps[1].page, ext.steps[1].lo, ext.steps[1].hi),
            (2, 0, 4)
        );
        let text = copy_steps(&ext, |p| cache.get(&p).map(|r| layer_flat(r, Layer::Body))).unwrap();
        assert_eq!(text, "b c\nd e f");

        // 后向:锚点页 2 的 "e"(idx2) → 页 1 的 "c"(idx4)。
        let sel = selection_with_anchor(2, Layer::Body, 2);
        let ext = shift_extend_selection(&cache, &sel, 1, Layer::Body, 4).unwrap();
        assert_eq!(
            (ext.steps[0].page, ext.steps[0].lo, ext.steps[0].hi),
            (1, 4, 4)
        );
        assert_eq!(
            (ext.steps[1].page, ext.steps[1].lo, ext.steps[1].hi),
            (2, 0, 2)
        );
        let text = copy_steps(&ext, |p| cache.get(&p).map(|r| layer_flat(r, Layer::Body))).unwrap();
        assert_eq!(text, "c\nd e");
    }

    #[test]
    fn shift_extend_rejects_layer_span_and_missing_pages() {
        let mut cache = HashMap::new();
        cache.insert(1, page_with_words(&["a", "b"]));
        cache.insert(2, page_with_words(&["c", "d"]));
        cache.insert(3, page_with_words(&["e", "f"]));

        let sel = selection_with_anchor(1, Layer::Body, 0);
        assert!(shift_extend_selection(&cache, &sel, 1, Layer::Small, 0).is_none());
        assert!(shift_extend_selection(&cache, &sel, 3, Layer::Body, 0).is_none());

        let sel = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            anchor: Some((9, Layer::Body, 0)),
            steps: Vec::new(),
        };
        assert!(shift_extend_selection(&cache, &sel, 1, Layer::Body, 0).is_none());

        let sel = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            anchor: None,
            steps: Vec::new(),
        };
        assert!(shift_extend_selection(&cache, &sel, 1, Layer::Body, 0).is_none());
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
            anchor: None,
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
            anchor: None,
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
            anchor: None,
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
            anchor: None,
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
    fn paragraph_range_keeps_short_glyph_line_within_paragraph() {
        // “canvases.” 场景:前一行盒高 1.46、全小写行盒高 1.08,
        // 行距 2.96 与列中位行距 2.96 一致 → 同一段;
        // 段间距 4.16 超过 1.35× 行距 → 另起段。
        let flat = vec![
            fw(0, "with", 8.0, 40.0, 1.46),
            fw(0, " ", 9.0, 40.0, 1.46),
            fw(0, "connected", 10.0, 40.0, 1.46),
            fw(1, "canvases.", 8.0, 42.96, 1.08),
            fw(2, "Next", 8.0, 47.12, 1.46),
        ];
        assert_eq!(paragraph_range(&flat, 0), (0, 3), "短字形行仍属同一段");
        assert_eq!(paragraph_range(&flat, 4), (4, 4), "段间距处另起段");
    }

    #[test]
    fn current_page_geometry_hits_viewport_center() {
        // 页高 800/800/500（page_w=1 时），页间距 28。
        let prefix = vec![0.0, 800.0, 1600.0, 2100.0];
        assert_eq!(current_page_from_prefix(&prefix, 1.0, 28.0, 0.0, 400.0), 1);
        // 第 2 页顶 = 800 + 28 = 828
        assert_eq!(
            current_page_from_prefix(&prefix, 1.0, 28.0, 956.0, 400.0),
            2
        );
        // 第 3 页顶 = 1600 + 56 = 1656
        assert_eq!(
            current_page_from_prefix(&prefix, 1.0, 28.0, 1784.0, 400.0),
            3
        );
        // 视口中心超过最后页 → 停在末页
        assert_eq!(
            current_page_from_prefix(&prefix, 1.0, 28.0, 99999.0, 400.0),
            3
        );
    }

    #[test]
    fn spacer_heights_use_prefix_and_gaps() {
        // 每页比例 1.0，page_w=100 → 每页高 100，gap=28。
        let prefix = vec![0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0];
        // 窗口 3..=5：顶部 1..2 两页（100+100+28），底部仅第 6 页（100）。
        assert_eq!(spacer_heights(&prefix, 100.0, 28.0, 3, 5), (228.0, 100.0));
        // 无顶部/无底部
        assert_eq!(spacer_heights(&prefix, 100.0, 28.0, 1, 6), (0.0, 0.0));
        // 底部 3 页（4..6）：3*100 + 2*28 = 356
        assert_eq!(spacer_heights(&prefix, 100.0, 28.0, 1, 3), (0.0, 356.0));
    }

    #[test]
    fn page_ratio_is_prefix_difference() {
        let prefix = vec![0.0, 0.5, 1.0, 2.2];
        assert_eq!(page_ratio_at(&prefix, 1), 0.5);
        assert!(
            (page_ratio_at(&prefix, 3) - 1.2).abs() < 1e-9,
            "前缀差分应为 1.2"
        );
        assert_eq!(page_ratio_at(&prefix, 99), 1.0, "越界回退默认比例");
    }

    #[test]
    fn placeholder_aspect_is_width_over_height() {
        let prefix = vec![0.0, 0.5, 1.0, 2.2];
        // h/w = 0.5 → aspect-ratio（width/height）= 2.0
        assert!((page_aspect_at(&prefix, 1) - 2.0).abs() < 1e-9);
        // h/w = 1.2 → aspect-ratio ≈ 0.8333
        assert!((page_aspect_at(&prefix, 3) - 1.0 / 1.2).abs() < 1e-9);
        assert_eq!(page_aspect_at(&prefix, 99), 1.0, "越界回退默认比例");
    }

    #[test]
    fn window_bounds_clamp_to_document() {
        assert_eq!(window_bounds(1, 3, 15), (1, 3));
        assert_eq!(window_bounds(50, 100, 15), (35, 65));
        assert_eq!(window_bounds(100, 100, 15), (85, 100));
        assert_eq!(window_bounds(0, 0, 15), (1, 1));
    }

    #[test]
    fn eviction_keeps_current_and_selection_pages() {
        fn page(n: u32) -> RenderedPage {
            RenderedPage {
                src: n.to_string(),
                body: Vec::new(),
                small: Vec::new(),
                w_pt: 1.0,
                h_pt: 1.0,
                warning: None,
            }
        }
        let mut cache: HashMap<u32, RenderedPage> = (1..=10).map(|p| (p, page(p))).collect();
        let selection = Selection {
            layer: Layer::Body,
            formula: false,
            formula_score: 0.0,
            anchor: None,
            steps: vec![SelectionStep {
                page: 2,
                lo: 0,
                hi: 0,
                column_left: None,
            }],
        };
        evict_cache_pages(&mut cache, 6, Some(&selection), 4);
        assert!(cache.contains_key(&6), "当前页保留");
        assert!(cache.contains_key(&2), "选区页保留");
        assert!(
            cache.contains_key(&5) && cache.contains_key(&7),
            "最近邻居优先"
        );
        assert_eq!(cache.len(), 4);
        assert!(
            !cache.contains_key(&1) && !cache.contains_key(&10),
            "最远页先淘汰"
        );
    }

    #[test]
    fn copy_text_keeps_short_glyph_soft_wrap_as_space() {
        let flat = vec![
            fw(0, "connected", 8.0, 40.0, 1.46),
            fw(1, "canvases.", 8.0, 42.96, 1.08),
        ];
        assert_eq!(copy_text_filtered(&flat, 0, 1, None), "connected canvases.");
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
            font_size_pt: 10.0,
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
    fn calibrated_small_font_ratio_keeps_mid_size_lines_in_body() {
        let overlay = vec![
            overlay_line(vec![("Hello".into(), 10.0, 0.0, 20.0, 2.0)]),
            overlay_line(vec![("World".into(), 10.0, 3.0, 20.0, 2.0)]),
            overlay_line(vec![("More".into(), 10.0, 6.0, 20.0, 2.0)]),
            overlay_line(vec![("Text".into(), 10.0, 9.0, 20.0, 2.0)]),
            overlay_line_with_font(vec![("note".into(), 10.0, 12.0, 20.0, 2.0)], 8.0),
        ];
        let (_, small_default) = classify_words(&overlay);
        assert!(
            small_default.iter().any(|w| w.text == "note"),
            "默认小字比例 0.91 把 0.8×中位字号判为小字"
        );
        let cal = crate::pdf::calibration::DocCalibration {
            small_height_ratio: 0.75,
            ..Default::default()
        };
        let (body_cal, small_cal) = classify_words_with(&overlay, cal);
        assert!(body_cal.iter().any(|w| w.text == "note"));
        assert!(!small_cal.iter().any(|w| w.text == "note"));
    }

    #[test]
    fn classify_keeps_x_height_body_line_in_body_when_font_matches() {
        // “canvases.” 这类全小写行:字形包围盒偏矮(1.0 vs 2.0),
        // 但真实字号与正文相同(10pt),应留在正文层;真正的小字(8pt)才进小字层。
        let overlay = vec![
            overlay_line(vec![("Hello".into(), 10.0, 0.0, 20.0, 2.0)]),
            overlay_line(vec![("World".into(), 10.0, 3.0, 20.0, 2.0)]),
            overlay_line(vec![("More".into(), 10.0, 6.0, 20.0, 2.0)]),
            overlay_line(vec![("Text".into(), 10.0, 9.0, 20.0, 2.0)]),
            overlay_line_with_font(vec![("canvases.".into(), 10.0, 12.0, 20.0, 1.0)], 10.0),
            overlay_line_with_font(vec![("footnote".into(), 10.0, 15.0, 20.0, 2.0)], 8.0),
        ];
        let (body, small) = classify_words(&overlay);
        assert!(
            body.iter().any(|w| w.text == "canvases."),
            "同字号全小写行不应进小字层"
        );
        assert!(!small.iter().any(|w| w.text == "canvases."));
        assert!(
            small.iter().any(|w| w.text == "footnote"),
            "真小字仍应进小字层"
        );
    }

    #[test]
    fn classify_falls_back_to_height_when_font_missing() {
        // 字号信息缺失(0)时退回旧的行高比例判定,保证退化页面仍能区分小字。
        let overlay = vec![
            overlay_line_with_font(vec![("Hello".into(), 10.0, 0.0, 20.0, 2.0)], 0.0),
            overlay_line_with_font(vec![("World".into(), 10.0, 3.0, 20.0, 2.0)], 0.0),
            overlay_line_with_font(vec![("More".into(), 10.0, 6.0, 20.0, 2.0)], 0.0),
            overlay_line_with_font(vec![("Text".into(), 10.0, 9.0, 20.0, 2.0)], 0.0),
            overlay_line_with_font(vec![("note".into(), 10.0, 12.0, 20.0, 1.6)], 0.0),
        ];
        let (_, small_default) = classify_words(&overlay);
        assert!(
            small_default.iter().any(|w| w.text == "note"),
            "字号缺失时应按行高比例(0.8×)判为小字"
        );
        let cal = crate::pdf::calibration::DocCalibration {
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
        let cal = crate::pdf::calibration::DocCalibration {
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
        let cal = crate::pdf::calibration::DocCalibration {
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
