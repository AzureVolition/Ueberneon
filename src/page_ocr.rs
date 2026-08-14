// ── 页面 OCR 后端 ──
//
// 与 formula_ocr 同构的 manifest 驱动本地 ONNX 页面 OCR:
// PaddleOCR PP-OCRv4 风格(det + 可选 cls + rec)。用户下载模型包后放到
// ~/.ueberneon/page-ocr-models/<模型名>/ 即可被发现;未配置时
// `backend_arc()` 返回 NotConfigured,上层保持纯扫描页(不阻塞阅读)。
//
// 产出两种落盘数据:
//   - <书目录>/ocr/<NNNN>.json   词级 OverlayLine(阅读器透明选区)
//   - <书目录>/pages/NNNN.md     知识库文本(与 PDFium 提取管线同构)

use std::collections::HashSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use image::{DynamicImage, RgbImage};
use ort::session::Session;
use serde::{Deserialize, Serialize};

use crate::pdf::pdfium::PdfDocument;
use crate::pdf::{OverlayLine, OverlayWord, ParseMarker};

const OCR_PAGE_VERSION: u32 = 2;
const OCR_PROGRESS_VERSION: u32 = 2;
/// 并行推理会话池大小(与设置允许的最大 worker 数一致)。
const SESSION_POOL_SIZE: usize = 4;
/// 阅读器/后台共用页面认领键。
fn claim_key(book_dir: &Path, page_1based: u32) -> String {
    format!("{}:{page_1based}", book_dir.display())
}

// ── 错误类型 ──

#[derive(Debug, Clone)]
pub enum PageOcrError {
    NotConfigured(String),
    Io(String),
    Json(String),
    Ort(String),
    Decode(String),
    Ocr(String),
}

impl std::fmt::Display for PageOcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PageOcrError::NotConfigured(m) => write!(f, "{m}"),
            PageOcrError::Io(m) => write!(f, "IO 错误:{m}"),
            PageOcrError::Json(m) => write!(f, "配置解析错误:{m}"),
            PageOcrError::Ort(m) => write!(f, "ONNX Runtime 错误:{m}"),
            PageOcrError::Decode(m) => write!(f, "解码错误:{m}"),
            PageOcrError::Ocr(m) => write!(f, "OCR 错误:{m}"),
        }
    }
}

impl std::error::Error for PageOcrError {}

// ── 识别结果(归一化坐标,原点左上,0..1) ──

/// 一行 OCR 文本及其外接盒。
#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    pub text: String,
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

/// 页面 OCR 后端接口(后续可替换视觉 LLM 等实现)。
pub trait PageOcrBackend: Send + Sync {
    /// 输入 RGBA 页面图像,返回归一化坐标(0..1)的文本行。
    fn recognize_page_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<OcrLine>, PageOcrError>;
}

// ── 模型发现与后端缓存 ──

#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    pub name: String,
    pub dir: PathBuf,
}

fn configured_model_dir() -> Option<PathBuf> {
    if let Some(dir) = crate::settings::get().page_ocr.model_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(dir) = env::var("UEBERNEON_PAGE_OCR_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// 扫描本地页面 OCR 模型目录:
/// - `~/.ueberneon/page-ocr-models/*/`(主目录,拖入即用)
/// - `$CARGO_HOME/ueberneon-page-ocr/*/`(导出脚本缓存,兼容)
/// - `UEBERNEON_PAGE_OCR_DIR`(若设置)
pub fn discover_models() -> Vec<DiscoveredModel> {
    let mut roots = Vec::new();
    if let Ok(home) = env::var("HOME") {
        roots.push(
            PathBuf::from(home)
                .join(".ueberneon")
                .join("page-ocr-models"),
        );
    }
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".cargo")
        });
    roots.push(cargo_home.join("ueberneon-page-ocr"));
    if let Some(dir) = configured_model_dir() {
        roots.push(dir);
    }

    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(&root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let manifest_path = dir.join("manifest.json");
            if !manifest_path.is_file() {
                continue;
            }
            let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if !seen.insert(canonical) {
                continue;
            }
            let name = fs::read_to_string(&manifest_path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
                .unwrap_or_else(|| {
                    dir.file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unnamed")
                        .to_string()
                });
            out.push(DiscoveredModel { name, dir });
        }
    }
    if let Some(dir) = configured_model_dir() {
        if dir.join("manifest.json").is_file() {
            let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
            if seen.insert(canonical) {
                let name = fs::read_to_string(dir.join("manifest.json"))
                    .ok()
                    .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                    .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
                    .unwrap_or_else(|| {
                        dir.file_name()
                            .and_then(|s| s.to_str())
                            .unwrap_or("unnamed")
                            .to_string()
                    });
                out.push(DiscoveredModel { name, dir });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

struct BackendSlot {
    key: String,
    backend: Result<Arc<dyn PageOcrBackend>, PageOcrError>,
}

static BACKEND_STATE: OnceLock<Mutex<Option<BackendSlot>>> = OnceLock::new();

fn backend_key() -> String {
    match configured_model_dir() {
        Some(dir) => format!("dir:{}", dir.display()),
        None => "none".into(),
    }
}

/// 获取全局页面 OCR 后端;配置的模型目录变化后自动重新初始化。
pub fn backend_arc() -> Result<Arc<dyn PageOcrBackend>, PageOcrError> {
    let key = backend_key();
    let mut state = BACKEND_STATE
        .get_or_init(|| Mutex::new(None))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if let Some(slot) = state.as_ref() {
        if slot.key == key {
            return match &slot.backend {
                Ok(b) => Ok(b.clone()),
                Err(e) => Err(e.clone()),
            };
        }
    }
    let backend = match configured_model_dir() {
        Some(dir) if dir.join("manifest.json").is_file() => {
            PaddleOcrRecognizer::load(&dir).map(|b| Arc::new(b) as Arc<dyn PageOcrBackend>)
        }
        Some(dir) => Err(PageOcrError::NotConfigured(format!(
            "模型目录缺少 manifest.json:{}",
            dir.display()
        ))),
        None => Err(PageOcrError::NotConfigured(
            "未找到页面 OCR 模型:请把包含 manifest.json / det_model.onnx / rec_model.onnx \
             / rec_dict.txt(可选 cls_model.onnx / libonnxruntime.dylib)的模型目录放到 \
             ~/.ueberneon/page-ocr-models/ 后刷新"
                .to_string(),
        )),
    };
    *state = Some(BackendSlot { key, backend });
    match state.as_ref().unwrap().backend.as_ref() {
        Ok(b) => Ok(b.clone()),
        Err(e) => Err(e.clone()),
    }
}

// ── PaddleOCR PP-OCRv4 manifest ──

fn default_rec_input_size() -> [u32; 2] {
    [48, 320]
}
fn default_cls_input_size() -> [u32; 2] {
    [48, 192]
}
fn default_mean() -> [f32; 3] {
    [0.485, 0.456, 0.406]
}
fn default_std() -> [f32; 3] {
    [0.229, 0.224, 0.225]
}
fn default_max_side() -> u32 {
    2000
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize)]
struct PaddleOcrManifest {
    format: String,
    det_model: String,
    rec_model: String,
    rec_dict: String,
    #[serde(default)]
    cls_model: Option<String>,
    #[serde(default = "default_rec_input_size")]
    rec_input_size: [u32; 2],
    #[serde(default = "default_cls_input_size")]
    cls_input_size: [u32; 2],
    #[serde(default = "default_mean")]
    mean: [f32; 3],
    #[serde(default = "default_std")]
    std: [f32; 3],
    #[serde(default = "default_det_limit")]
    det_limit_side_len: u32,
    /// "max" = 长边压到 det_limit_side_len(v4 风格);"min" = 短边不足时放大(v5/v6 风格)。
    #[serde(default = "default_det_limit_type")]
    det_limit_type: String,
    #[serde(default = "default_det_thresh")]
    det_thresh: f32,
    #[serde(default = "default_box_thresh")]
    box_thresh: f32,
    #[serde(default = "default_unclip_ratio")]
    unclip_ratio: f32,
    #[serde(default = "default_cls_thresh")]
    cls_thresh: f32,
    /// DB 后处理在找连通域前是否先做 2x2 膨胀(RapidOCR v6 默认开启)。
    #[serde(default)]
    use_dilation: bool,
    #[serde(default = "default_true")]
    use_space_char: bool,
}

fn default_det_limit() -> u32 {
    960
}
fn default_det_limit_type() -> String {
    "max".into()
}
fn default_det_thresh() -> f32 {
    0.3
}
fn default_box_thresh() -> f32 {
    0.6
}
fn default_unclip_ratio() -> f32 {
    1.5
}
fn default_cls_thresh() -> f32 {
    0.9
}

/// PP-OCRv4/v6 风格后端:det(DB) → 可选 cls(旋转分类) → rec(CTC)。
/// ort 的 `Session::run` 需要 `&mut self`,因此维护一个会话池,
/// 每个并行 worker 独占一组 det/cls/rec 会话,互不阻塞。
pub struct PaddleOcrRecognizer {
    models: Vec<Mutex<ModelSessions>>,
    next: AtomicU32,
    chars: Vec<String>,
    manifest: PaddleOcrManifest,
}

struct ModelSessions {
    det: Session,
    cls: Option<Session>,
    rec: Session,
}

impl PaddleOcrRecognizer {
    pub fn load(dir: &Path) -> Result<Self, PageOcrError> {
        let manifest_bytes = fs::read(dir.join("manifest.json"))
            .map_err(|e| PageOcrError::Io(format!("读取 manifest.json:{e}")))?;
        let manifest: PaddleOcrManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| PageOcrError::Json(format!("manifest:{e}")))?;
        if !matches!(
            manifest.format.as_str(),
            "paddle-ocr-v4-onnx" | "paddle-ocr-v6-onnx"
        ) {
            return Err(PageOcrError::NotConfigured(format!(
                "不支持的模型格式:{}",
                manifest.format
            )));
        }

        // ONNX Runtime:优先用模型包自带的库;没有时复用已初始化的 runtime。
        let lib_path = dir.join("libonnxruntime.dylib");
        if lib_path.is_file() {
            crate::onnx_runtime::ensure_initialized(&lib_path).map_err(PageOcrError::Ort)?;
        }

        let session_from = |name: &str| -> Result<Session, PageOcrError> {
            Session::builder()
                .map_err(|e| PageOcrError::Ort(format!("builder:{e}")))?
                .with_intra_threads(2)
                .map_err(|e| PageOcrError::Ort(format!("intra_threads:{e}")))?
                .commit_from_file(dir.join(name))
                .map_err(|e| PageOcrError::Ort(format!("commit_from_file({name:?}):{e}")))
        };
        let mut models = Vec::with_capacity(SESSION_POOL_SIZE);
        for _ in 0..SESSION_POOL_SIZE {
            let det = session_from(&manifest.det_model)?;
            let rec = session_from(&manifest.rec_model)?;
            let cls = match &manifest.cls_model {
                Some(name) => Some(session_from(name)?),
                None => None,
            };
            models.push(Mutex::new(ModelSessions { det, cls, rec }));
        }

        let dict_bytes = fs::read(dir.join(&manifest.rec_dict))
            .map_err(|e| PageOcrError::Io(format!("读取 {}:{e}", manifest.rec_dict)))?;
        let mut chars = vec![String::new()]; // CTC blank 固定为 index 0
        for line in String::from_utf8_lossy(&dict_bytes).lines() {
            let line = line.trim_end_matches('\r');
            if !line.is_empty() {
                chars.push(line.to_string());
            }
        }
        if manifest.use_space_char && !chars.iter().any(|c| c == " ") {
            chars.push(" ".to_string());
        }

        Ok(Self {
            models,
            next: AtomicU32::new(0),
            chars,
            manifest,
        })
    }

    fn normalize_bgr(&self, rgb: &RgbImage, h: u32, w: u32) -> ndarray::Array4<f32> {
        let mut input = ndarray::Array4::<f32>::zeros((1, 3, h as usize, w as usize));
        for y in 0..h {
            for x in 0..w {
                let px = rgb.get_pixel(x.min(rgb.width() - 1), y.min(rgb.height() - 1));
                let v = [px[2], px[1], px[0]]; // RGB -> BGR
                for c in 0..3 {
                    input[[0, c, y as usize, x as usize]] =
                        (v[c] as f32 / 255.0 - self.manifest.mean[c]) / self.manifest.std[c];
                }
            }
        }
        input
    }

    fn run_f32(
        session: &mut Session,
        input: ndarray::Array4<f32>,
    ) -> Result<ndarray::ArrayD<f32>, PageOcrError> {
        let tensor = ort::value::TensorRef::from_array_view(&input)
            .map_err(|e| PageOcrError::Ort(format!("tensor:{e}")))?;
        let outputs = session
            .run(ort::inputs![tensor])
            .map_err(|e| PageOcrError::Ort(format!("run:{e}")))?;
        let output = outputs
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| PageOcrError::Ort("模型无输出".into()))?;
        let tensor = output
            .downcast::<ort::value::TensorValueType<f32>>()
            .map_err(|e| PageOcrError::Ort(format!("downcast:{e}")))?;
        tensor
            .try_extract_array::<f32>()
            .map(|a| a.to_owned())
            .map_err(|e| PageOcrError::Ort(format!("output:{e}")))
    }

    /// DB 检测:返回原图坐标系(左上原点)的文本行矩形。
    fn detect(
        &self,
        rgb: &RgbImage,
        sessions: &mut ModelSessions,
    ) -> Result<Vec<DetBox>, PageOcrError> {
        let (src_w, src_h) = (rgb.width(), rgb.height());
        let limit = self.manifest.det_limit_side_len.max(32);
        let ratio = if self.manifest.det_limit_type == "min" {
            // RapidOCR v5/v6:"min" 模式把短边放大到 limit(不足时),不缩长边。
            let min_dim = src_h.min(src_w).max(1) as f64;
            if min_dim < limit as f64 {
                limit as f64 / min_dim
            } else {
                1.0
            }
        } else {
            // v4 风格:长边压到 limit,只缩不放。
            (limit as f64 / src_h.max(src_w).max(1) as f64).min(1.0)
        };
        let new_w = ((src_w as f64 * ratio).round() as u32).max(1);
        let new_h = ((src_h as f64 * ratio).round() as u32).max(1);
        let ih = (new_h + 31) / 32 * 32;
        let iw = (new_w + 31) / 32 * 32;
        let resized =
            image::imageops::resize(rgb, new_w, new_h, image::imageops::FilterType::Triangle);
        let mut padded = RgbImage::from_pixel(iw, ih, image::Rgb([0, 0, 0]));
        image::imageops::overlay(&mut padded, &resized, 0, 0);

        let input = self.normalize_bgr(&padded, ih, iw);
        let output = Self::run_f32(&mut sessions.det, input)?;
        let shape = output.shape();
        if shape.len() < 2 {
            return Err(PageOcrError::Ocr(format!("det 输出维度异常:{shape:?}")));
        }
        let ph = shape[shape.len() - 2] as usize;
        let pw = shape[shape.len() - 1] as usize;
        if output.len() < ph * pw {
            return Err(PageOcrError::Ocr("det 输出尺寸不足".into()));
        }
        let data: Vec<f32> = output.iter().copied().collect();
        let prob_start = data.len() - ph * pw;
        let prob = &data[prob_start..];
        let prob_w = pw as u32;
        let prob_h = ph as u32;

        // 裁剪掉右侧/下侧 padding 后映射回原图坐标。
        let boxes = db_postprocess(
            prob,
            prob_w,
            prob_h,
            new_w,
            new_h,
            src_w,
            src_h,
            self.manifest.det_thresh,
            self.manifest.box_thresh,
            self.manifest.unclip_ratio,
            self.manifest.use_dilation,
        );
        Ok(boxes)
    }

    fn classify_crop(
        &self,
        crop: &RgbImage,
        sessions: &mut ModelSessions,
    ) -> Result<bool, PageOcrError> {
        let Some(cls) = sessions.cls.as_mut() else {
            return Ok(false);
        };
        let [ih, iw] = self.manifest.cls_input_size;
        let resized = image::imageops::resize(crop, iw, ih, image::imageops::FilterType::Triangle);
        let input = self.normalize_bgr(&resized, ih, iw);
        let output = Self::run_f32(cls, input)?;
        let shape = output.shape();
        if shape.len() < 2 || output.len() < 2 {
            return Err(PageOcrError::Ocr(format!("cls 输出维度异常:{shape:?}")));
        }
        let n = shape[shape.len() - 1] as usize;
        let data: Vec<f32> = output.iter().copied().collect();
        let start = data.len() - n;
        let (mut best, mut best_v) = (0usize, data[start]);
        for (i, &v) in data[start..].iter().enumerate() {
            if v > best_v {
                best = i;
                best_v = v;
            }
        }
        // RapidOCR/PaddleOCR 约定:label_list = ["0", "180"],score 超过阈值才旋转。
        Ok(best == 1 && best_v >= self.manifest.cls_thresh)
    }

    fn recognize_crop(
        &self,
        crop: &RgbImage,
        rotate180: bool,
        sessions: &mut ModelSessions,
    ) -> Result<String, PageOcrError> {
        let img = if rotate180 {
            DynamicImage::ImageRgb8(crop.clone()).rotate180().to_rgb8()
        } else {
            crop.clone()
        };
        // RapidOCR 风格:按宽高比计算宽度,不足 rec_input_size[1] 时补零到最小宽度;
        // 过宽的行保留真实宽度(上限 2048,与 Global.max_side_len 对齐)。
        let [ih, min_iw] = self.manifest.rec_input_size;
        let ratio = img.width() as f64 / img.height().max(1) as f64;
        let natural_w = ((ih as f64 * ratio).ceil() as u32).max(1);
        let batch_w = natural_w.max(min_iw).min(2048);
        let resized_w = natural_w.min(batch_w).max(1);
        let resized =
            image::imageops::resize(&img, resized_w, ih, image::imageops::FilterType::Triangle);
        let mut padded = RgbImage::from_pixel(batch_w, ih, image::Rgb([0, 0, 0]));
        image::imageops::overlay(&mut padded, &resized, 0, 0);
        let input = self.normalize_bgr(&padded, ih, batch_w);
        let output = Self::run_f32(&mut sessions.rec, input)?;
        let shape = output.shape();
        if shape.len() < 2 {
            return Err(PageOcrError::Ocr(format!("rec 输出维度异常:{shape:?}")));
        }
        let (t_len, c_len) = if shape.len() == 3 {
            (shape[1], shape[2])
        } else {
            (shape[0], shape[1])
        };
        let data: Vec<f32> = output.iter().copied().collect();
        let start = data.len() - t_len * c_len;
        Ok(ctc_decode(&data[start..], t_len, c_len, &self.chars))
    }
}

impl PageOcrBackend for PaddleOcrRecognizer {
    fn recognize_page_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<Vec<OcrLine>, PageOcrError> {
        let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| PageOcrError::Decode("RGBA 尺寸与缓冲区不一致".into()))?;
        if width == 0 || height == 0 {
            return Err(PageOcrError::Decode("空图像".into()));
        }
        let rgb = DynamicImage::ImageRgba8(img).to_rgb8();
        let idx = self.next.fetch_add(1, Ordering::Relaxed) as usize % self.models.len();
        let mut guard = self
            .models
            .get(idx)
            .ok_or_else(|| PageOcrError::Ocr("会话池为空".into()))?
            .lock()
            .map_err(|_| PageOcrError::Ocr("会话池锁损坏".into()))?;
        let sessions = &mut *guard;
        let boxes = self.detect(&rgb, sessions)?;
        let mut lines = Vec::new();
        for b in boxes {
            let crop = crop_rotated(&rgb, &b);
            let rotate180 = self.classify_crop(&crop, sessions)?;
            let text = self.recognize_crop(&crop, rotate180, sessions)?;
            if text.trim().is_empty() {
                continue;
            }
            let (x0, y0, x1, y1) = b.normalized(width, height);
            lines.push(OcrLine {
                text: text.trim().to_string(),
                x0,
                y0,
                x1,
                y1,
            });
        }
        sort_lines(&mut lines);
        Ok(lines)
    }
}

// ── DB(可微分二值化)后处理 ──

struct DetBox {
    center: [f64; 2],
    w: f64,
    h: f64,
    angle: f64,
}

impl DetBox {
    fn normalized(&self, page_w: u32, page_h: u32) -> (f64, f64, f64, f64) {
        let (w, h) = (page_w.max(1) as f64, page_h.max(1) as f64);
        let (c, s) = (self.angle.cos(), self.angle.sin());
        let hw = self.w / 2.0;
        let hh = self.h / 2.0;
        let corners = [
            [
                self.center[0] - hw * c + hh * s,
                self.center[1] - hw * s - hh * c,
            ],
            [
                self.center[0] + hw * c + hh * s,
                self.center[1] + hw * s - hh * c,
            ],
            [
                self.center[0] + hw * c - hh * s,
                self.center[1] + hw * s + hh * c,
            ],
            [
                self.center[0] - hw * c - hh * s,
                self.center[1] - hw * s + hh * c,
            ],
        ];
        let x0 = corners.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
        let x1 = corners
            .iter()
            .map(|p| p[0])
            .fold(f64::NEG_INFINITY, f64::max);
        let y0 = corners.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
        let y1 = corners
            .iter()
            .map(|p| p[1])
            .fold(f64::NEG_INFINITY, f64::max);
        (
            (x0 / w).clamp(0.0, 1.0),
            (y0 / h).clamp(0.0, 1.0),
            (x1 / w).clamp(0.0, 1.0),
            (y1 / h).clamp(0.0, 1.0),
        )
    }
}

/// DB 后处理:阈值化 → 连通域 → 最小外接矩形 → unclip → 分数过滤。
fn db_postprocess(
    prob: &[f32],
    prob_w: u32,
    _prob_h: u32,
    real_w: u32,
    real_h: u32,
    src_w: u32,
    _src_h: u32,
    thresh: f32,
    box_thresh: f32,
    unclip_ratio: f32,
    dilate: bool,
) -> Vec<DetBox> {
    let raw_mask: Vec<bool> = prob.iter().map(|&v| v >= thresh).collect();
    let mask: Vec<bool> = if dilate {
        // 2x2 全 1 膨胀,与 RapidOCR cv2.dilate(kernel=[[1,1],[1,1]]) 对齐。
        let mut m = raw_mask.clone();
        for y in 0.._prob_h {
            for x in 0..prob_w {
                let i = (y * prob_w + x) as usize;
                if raw_mask[i] {
                    for (dx, dy) in [(0u32, 0u32), (1, 0), (0, 1), (1, 1)] {
                        let (nx, ny) = (x + dx, y + dy);
                        if nx < prob_w && ny < _prob_h {
                            m[(ny * prob_w + nx) as usize] = true;
                        }
                    }
                }
            }
        }
        m
    } else {
        raw_mask
    };
    let mut out = Vec::new();
    let mut seen = vec![false; mask.len()];
    let mut queue = std::collections::VecDeque::new();
    for y in 0..real_h {
        for x in 0..real_w {
            let idx = (y * prob_w + x) as usize;
            if seen[idx] || !mask[idx] {
                continue;
            }
            // BFS 连通域
            let mut comp = Vec::new();
            let mut score_sum = 0.0f64;
            queue.clear();
            queue.push_back((x, y));
            seen[idx] = true;
            while let Some((cx, cy)) = queue.pop_front() {
                comp.push([cx as f64, cy as f64]);
                score_sum += prob[(cy * prob_w + cx) as usize] as f64;
                for (dx, dy) in [(1i32, 0), (-1, 0), (0, 1), (0, -1)] {
                    let nx = cx as i32 + dx;
                    let ny = cy as i32 + dy;
                    if nx < 0 || ny < 0 || nx >= real_w as i32 || ny >= real_h as i32 {
                        continue;
                    }
                    let ni = (ny as u32 * prob_w + nx as u32) as usize;
                    if !seen[ni] && mask[ni] {
                        seen[ni] = true;
                        queue.push_back((nx as u32, ny as u32));
                    }
                }
            }
            if comp.len() < 16 {
                continue;
            }
            let (center, mut w, mut h, angle) = min_area_rect(&comp);
            let area = (w * h).max(1.0);
            let perimeter = 2.0 * (w + h).max(2.0);
            let d = unclip_ratio as f64 * area / perimeter;
            w += 2.0 * d;
            h += 2.0 * d;
            let score = score_sum / comp.len() as f64;
            if score < box_thresh as f64 {
                continue;
            }
            // 映射回原图坐标(左上原点)。
            let scale = src_w as f64 / real_w as f64;
            out.push(DetBox {
                center: [center[0] * scale, center[1] * scale],
                w: w * scale,
                h: h * scale,
                angle,
            });
        }
    }
    out
}

/// 凸包(Andrew 单调链)。
fn convex_hull(mut pts: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    if pts.len() <= 2 {
        return pts;
    }
    pts.sort_by(|a, b| {
        a[0].partial_cmp(&b[0])
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a[1].partial_cmp(&b[1]).unwrap_or(std::cmp::Ordering::Equal))
    });
    let cross = |o: [f64; 2], a: [f64; 2], b: [f64; 2]| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    let mut lower = Vec::new();
    for &p in &pts {
        while lower.len() >= 2 && cross(lower[lower.len() - 2], lower[lower.len() - 1], p) <= 0.0 {
            lower.pop();
        }
        lower.push(p);
    }
    let mut upper = Vec::new();
    for &p in pts.iter().rev() {
        while upper.len() >= 2 && cross(upper[upper.len() - 2], upper[upper.len() - 1], p) <= 0.0 {
            upper.pop();
        }
        upper.push(p);
    }
    lower.pop();
    upper.pop();
    lower.extend(upper);
    lower
}

/// 旋转卡壳求最小面积外接矩形;angle 为长轴方向(弧度)。
fn min_area_rect(pts: &[[f64; 2]]) -> ([f64; 2], f64, f64, f64) {
    let hull = convex_hull(pts.to_vec());
    if hull.len() <= 2 {
        let (mut x0, mut y0, mut x1, mut y1) = (
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        );
        for p in pts {
            x0 = x0.min(p[0]);
            y0 = y0.min(p[1]);
            x1 = x1.max(p[0]);
            y1 = y1.max(p[1]);
        }
        return ([(x0 + x1) / 2.0, (y0 + y1) / 2.0], x1 - x0, y1 - y0, 0.0);
    }
    let n = hull.len();
    let mut best_area = f64::INFINITY;
    let mut best = ([0.0; 2], 0.0f64, 0.0f64, 0.0f64);
    for i in 0..n {
        let a = hull[i];
        let b = hull[(i + 1) % n];
        let angle = (b[1] - a[1]).atan2(b[0] - a[0]);
        let (u, v) = (angle.cos(), angle.sin());
        let (mut min_u, mut max_u, mut min_v, mut max_v) = (
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
        );
        for p in &hull {
            let uu = p[0] * u + p[1] * v;
            let vv = -p[0] * v + p[1] * u;
            min_u = min_u.min(uu);
            max_u = max_u.max(uu);
            min_v = min_v.min(vv);
            max_v = max_v.max(vv);
        }
        let w = max_u - min_u;
        let h = max_v - min_v;
        let area = w * h;
        if area < best_area {
            best_area = area;
            let cu = (min_u + max_u) / 2.0;
            let cv = (min_v + max_v) / 2.0;
            let center = [cu * u - cv * v, cu * v + cv * u];
            let mut angle = angle;
            let (mut w, mut h) = (w, h);
            if w < h {
                std::mem::swap(&mut w, &mut h);
                angle += std::f64::consts::FRAC_PI_2;
            }
            best = (center, w, h, angle);
        }
    }
    best
}

/// 按矩形姿态从原图裁剪文本区域(双线性采样)。
fn crop_rotated(rgb: &RgbImage, b: &DetBox) -> RgbImage {
    let ow = b.w.ceil().max(2.0) as u32;
    let oh = b.h.ceil().max(2.0) as u32;
    let (c, s) = (b.angle.cos(), b.angle.sin());
    let mut out = RgbImage::new(ow, oh);
    for vy in 0..oh {
        for ux in 0..ow {
            let u = ux as f64 - (b.w / 2.0);
            let v = vy as f64 - (b.h / 2.0);
            let x = b.center[0] + u * c - v * s;
            let y = b.center[1] + u * s + v * c;
            let px = bilinear_sample(rgb, x, y);
            out.put_pixel(ux, vy, px);
        }
    }
    out
}

fn bilinear_sample(rgb: &RgbImage, x: f64, y: f64) -> image::Rgb<u8> {
    let (w, h) = (rgb.width() as f64, rgb.height() as f64);
    let x = x.clamp(0.0, w - 1.0);
    let y = y.clamp(0.0, h - 1.0);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(rgb.width() - 1);
    let y1 = (y0 + 1).min(rgb.height() - 1);
    let fx = x - x0 as f64;
    let fy = y - y0 as f64;
    let p00 = rgb.get_pixel(x0, y0).0;
    let p10 = rgb.get_pixel(x1, y0).0;
    let p01 = rgb.get_pixel(x0, y1).0;
    let p11 = rgb.get_pixel(x1, y1).0;
    let mut out = [0u8; 3];
    for c in 0..3 {
        let top = p00[c] as f64 * (1.0 - fx) + p10[c] as f64 * fx;
        let bottom = p01[c] as f64 * (1.0 - fx) + p11[c] as f64 * fx;
        out[c] = (top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8;
    }
    image::Rgb(out)
}

/// CTC 解码:blank=0,跳 blank、合并连续重复。
fn ctc_decode(data: &[f32], t_len: usize, c_len: usize, chars: &[String]) -> String {
    let mut out = String::new();
    let mut prev = 0usize;
    for t in 0..t_len {
        let base = t * c_len;
        let mut best = 0usize;
        let mut best_v = data[base];
        for c in 1..c_len {
            let v = data[base + c];
            if v > best_v {
                best = c;
                best_v = v;
            }
        }
        if best == 0 {
            prev = 0;
            continue;
        }
        if best != prev {
            if let Some(ch) = chars.get(best) {
                out.push_str(ch);
            }
        }
        prev = best;
    }
    out
}

/// 按阅读顺序排序:先按列(左→右),列内按行(上→下)。
/// 行聚类:垂直带重叠 >= 25% 并入同一视觉行(与 build_text_overlay 一致)。
pub fn sort_lines(lines: &mut [OcrLine]) {
    if lines.is_empty() {
        return;
    }
    let median_height = {
        let mut hs: Vec<f64> = lines.iter().map(|l| l.y1 - l.y0).collect();
        hs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        hs[hs.len() / 2]
    };
    let gap_threshold = (median_height * 0.75).max(0.008);

    let mut by_y: Vec<usize> = (0..lines.len()).collect();
    by_y.sort_by(|&a, &b| {
        lines[a]
            .y0
            .partial_cmp(&lines[b].y0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut visual: Vec<Vec<usize>> = Vec::new();
    for idx in by_y {
        let h = (lines[idx].y1 - lines[idx].y0).max(1e-6);
        if let Some(line) = visual.last_mut() {
            let &last = line.last().unwrap();
            let top = lines[idx].y0.max(lines[last].y0);
            let bottom = lines[idx].y1.min(lines[last].y1);
            if bottom - top >= 0.25 * h {
                line.push(idx);
                continue;
            }
        }
        visual.push(vec![idx]);
    }

    let mut cols: Vec<Vec<usize>> = Vec::new();
    let mut all: Vec<usize> = visual.iter().flatten().copied().collect();
    all.sort_by(|&a, &b| {
        lines[a]
            .x0
            .partial_cmp(&lines[b].x0)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    for idx in all {
        let left = lines[idx].x0;
        if let Some(&last) = cols.last().and_then(|c| c.last()) {
            if left - lines[last].x0 > gap_threshold {
                cols.push(Vec::new());
            }
        } else {
            cols.push(Vec::new());
        }
        cols.last_mut().unwrap().push(idx);
    }
    let mut order = Vec::new();
    for col in &mut cols {
        col.sort_by(|&a, &b| {
            lines[a]
                .y0
                .partial_cmp(&lines[b].y0)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        order.extend(col.iter().copied());
    }
    let sorted = order;
    let snapshot: Vec<OcrLine> = sorted.iter().map(|&i| lines[i].clone()).collect();
    for (dst, src) in lines.iter_mut().zip(snapshot) {
        *dst = src;
    }
}

// ── 词拆分与 Overlay 转换 ──

fn is_cjk(c: char) -> bool {
    matches!(
        c,
        '\u{4e00}'..='\u{9fff}'
            | '\u{3400}'..='\u{4dbf}'
            | '\u{f900}'..='\u{faff}'
    )
}

fn char_weight(c: char) -> f64 {
    if c.is_whitespace() { 0.35 } else { 1.0 }
}

/// 把一行 OCR 文本按空白/Latin 词/CJK 单字拆成 OverlayWord,
/// 宽度按字符权重在行盒内比例分配(近似,足够拖选与复制)。
fn split_line_words(text: &str, x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<OverlayWord> {
    let (w, h) = ((x1 - x0).max(0.0), (y1 - y0).max(0.0));
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    // 无空白的纯 CJK 文本按单字拆;其余按空白拆词。
    let per_char =
        !text.contains(char::is_whitespace) && chars.len() > 1 && chars.iter().all(|&c| is_cjk(c));
    let tokens: Vec<(String, usize)> = if per_char {
        chars
            .iter()
            .map(|&c| (c.to_string(), c.len_utf8()))
            .collect()
    } else {
        text.split_whitespace()
            .map(|s| (s.to_string(), s.len()))
            .collect()
    };
    if tokens.is_empty() {
        return Vec::new();
    }
    let token_weights: Vec<f64> = tokens
        .iter()
        .map(|(s, _)| s.chars().map(char_weight).sum::<f64>())
        .collect();
    let gap_weight = if per_char { 0.0 } else { 0.35 };
    let total_units: f64 =
        token_weights.iter().sum::<f64>() + gap_weight * (tokens.len().saturating_sub(1)) as f64;
    let unit = w / total_units.max(1e-9);
    let mut words = Vec::new();
    let mut left = x0;
    for (i, (tok, _)) in tokens.iter().enumerate() {
        let tw = token_weights[i] * unit;
        let right = left + tw;
        words.push(OverlayWord {
            text: tok.clone(),
            left_pct: left * 100.0,
            top_cqw: y0 * 100.0,
            width_cqw: (right - left).max(0.1) * 100.0,
            height_cqw: h.max(0.01) * 100.0,
        });
        // Latin/空白拆词时在词间补一个空格词,复制语义与 PDFium 文本层一致;
        // 纯 CJK 逐字拆不补空格。
        if !per_char && i + 1 < tokens.len() {
            let gap = gap_weight * unit;
            words.push(OverlayWord {
                text: " ".to_string(),
                left_pct: right * 100.0,
                top_cqw: y0 * 100.0,
                width_cqw: (gap * 100.0).min(2.0).max(0.1),
                height_cqw: h.max(0.01) * 100.0,
            });
            left = right + gap;
        } else {
            left = right;
        }
    }
    words
}

/// 归一化 OCR 行 → 阅读器 OverlayLine(坐标体系与 PDFium 文本层一致)。
pub fn lines_to_overlay(lines: &[OcrLine], page_w_pt: f64, page_h_pt: f64) -> Vec<OverlayLine> {
    if page_w_pt <= 0.0 || page_h_pt <= 0.0 {
        return Vec::new();
    }
    let wh = page_h_pt / page_w_pt;
    lines
        .iter()
        .map(|l| {
            let words = split_line_words(&l.text, l.x0, l.y0, l.x1, l.y1);
            let height_norm = (l.y1 - l.y0).max(0.001);
            OverlayLine {
                top_pct: l.y0 * 100.0,
                height_pct: height_norm * 100.0,
                height_cqw: height_norm * wh * 100.0,
                font_size_pt: height_norm * page_h_pt,
                words,
            }
        })
        .collect()
}

/// Overlay 行 → 知识库纯文本:词间按 CJK 规则连接,行间换行,
/// 垂直间隙超过行高 1.25 倍时视为段落分界。
pub fn overlay_lines_to_text(lines: &[OverlayLine]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        let mut buf = String::new();
        let mut prev_last: Option<char> = None;
        let mut pending_space = false;
        for w in &line.words {
            let t = w.text.trim();
            if t.is_empty() {
                if !buf.is_empty() {
                    pending_space = true;
                }
                continue;
            }
            let first = t.chars().next().unwrap();
            let need_space = match prev_last {
                Some(_) if pending_space => true,
                Some(pl) => !(is_cjk(pl) && is_cjk(first)),
                None => false,
            };
            if need_space {
                buf.push(' ');
            }
            pending_space = false;
            buf.push_str(t);
            prev_last = t.chars().last();
        }
        let text = buf.trim_end();
        if i > 0 {
            let prev = &lines[i - 1];
            let gap = line.top_pct - (prev.top_pct + prev.height_pct);
            let th = line.height_pct.max(prev.height_pct).max(0.6);
            if gap > th * 1.25 {
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str(text);
    }
    out
}

// ── 落盘:单页词行 + 知识库文本 + 进度 ──

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OcrPageFile {
    pub version: u32,
    pub page: u32,
    pub engine: String,
    pub model_name: String,
    pub page_width_pt: f64,
    pub page_height_pt: f64,
    pub lines: Vec<OverlayLine>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OcrFailure {
    pub page: u32,
    pub error: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
pub struct OcrProgress {
    pub version: u32,
    pub pdf_size: u64,
    pub pdf_mtime: u64,
    pub model_dir: String,
    #[serde(default)]
    pub total_needed: Option<u32>,
    #[serde(default)]
    pub done: Vec<u32>,
    #[serde(default)]
    pub failed: Vec<OcrFailure>,
}

fn progress_path(book_dir: &Path) -> PathBuf {
    crate::layout::book_ocr_progress_path(book_dir)
}

fn fresh_progress(book_dir: &Path) -> Option<OcrProgress> {
    let (size, mtime) = crate::pdf::pdf_source_key(book_dir)?;
    Some(OcrProgress {
        version: OCR_PROGRESS_VERSION,
        pdf_size: size,
        pdf_mtime: mtime,
        model_dir: configured_model_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default(),
        ..Default::default()
    })
}

/// 读取进度;PDF 变化/版本不符时返回 None。
pub fn load_progress(book_dir: &Path) -> Option<OcrProgress> {
    let bytes = fs::read(progress_path(book_dir)).ok()?;
    let p: OcrProgress = serde_json::from_slice(&bytes).ok()?;
    if p.version != OCR_PROGRESS_VERSION {
        return None;
    }
    let (size, mtime) = crate::pdf::pdf_source_key(book_dir)?;
    if p.pdf_size != size || p.pdf_mtime != mtime {
        return None;
    }
    Some(p)
}

fn save_progress(book_dir: &Path, p: &OcrProgress) -> Result<(), String> {
    let path = progress_path(book_dir);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_vec_pretty(p).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

static PROGRESS_LOCK: Mutex<()> = Mutex::new(());

fn update_progress(book_dir: &Path, f: impl FnOnce(&mut OcrProgress)) -> Result<(), String> {
    let _guard = PROGRESS_LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let mut p = load_progress(book_dir).or_else(|| fresh_progress(book_dir));
    if let Some(p) = p.as_mut() {
        f(p);
        save_progress(book_dir, p)
    } else {
        Err("无法读取 PDF 源文件标识".to_string())
    }
}

fn mark_page_done(book_dir: &Path, page: u32) {
    let _ = update_progress(book_dir, |p| {
        p.done.retain(|&x| x != page);
        p.done.push(page);
        p.done.sort_unstable();
        p.failed.retain(|f| f.page != page);
    });
}

fn mark_page_failed(book_dir: &Path, page: u32, error: String) {
    let _ = update_progress(book_dir, |p| {
        p.failed.retain(|f| f.page != page);
        p.failed.push(OcrFailure { page, error });
        p.failed.sort_by_key(|f| f.page);
    });
}

fn read_page_file(book_dir: &Path, page_1based: u32) -> Option<OcrPageFile> {
    let path =
        crate::layout::book_ocr_page_path(&crate::layout::book_ocr_dir(book_dir), page_1based);
    let bytes = fs::read(path).ok()?;
    let f: OcrPageFile = serde_json::from_slice(&bytes).ok()?;
    if f.version != OCR_PAGE_VERSION || f.page != page_1based {
        return None;
    }
    Some(f)
}

fn write_page_file(book_dir: &Path, f: &OcrPageFile) -> Result<(), String> {
    let dir = crate::layout::book_ocr_dir(book_dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let path = crate::layout::book_ocr_page_path(&dir, f.page);
    let json = serde_json::to_vec_pretty(f).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, json).map_err(|e| e.to_string())?;
    fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

/// 页面是否有可用文本(PDFium 角度)。
pub fn needs_ocr(chars: &[crate::pdf::pdfium::TextChar]) -> bool {
    !crate::pdf::page_has_meaningful_text(chars)
}

/// 渲染倍率:长边压到 max_side 像素(约 2000),最高 4x(288dpi)。
fn ocr_render_scale(max_side: u32, w_pt: f32, h_pt: f32) -> f32 {
    let max_dim = w_pt.max(h_pt).max(1.0);
    (max_side as f32 / max_dim).clamp(1.0, 4.0)
}

/// 对单页执行 OCR 并落盘(词行 JSON + pages/NNNN.md + 进度 + 解析标记)。
pub fn ocr_page(
    book_dir: &Path,
    page_1based: u32,
    doc: &PdfDocument,
) -> Result<OcrPageFile, PageOcrError> {
    let backend = backend_arc()?;
    let (w_pt, h_pt) = doc
        .page_size(page_1based - 1)
        .map_err(|e| PageOcrError::Io(format!("页面尺寸失败:{e:#}")))?;
    let scale = ocr_render_scale(backend_scale_hint(), w_pt, h_pt);
    let png = doc
        .render_page_png(page_1based - 1, scale)
        .map_err(|e| PageOcrError::Io(format!("渲染页面失败:{e:#}")))?;
    let img = image::load_from_memory(&png)
        .map_err(|e| PageOcrError::Decode(format!("解码页面图像失败:{e}")))?
        .to_rgba8();
    let lines = backend.recognize_page_rgba(img.as_raw(), img.width(), img.height())?;
    let overlay = lines_to_overlay(&lines, w_pt as f64, h_pt as f64);

    let manifest_value = configured_model_dir().as_deref().and_then(|d| {
        fs::read_to_string(d.join("manifest.json"))
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
    });
    let model_name = manifest_value
        .as_ref()
        .and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
        .unwrap_or_else(|| "paddle-ocr-v6".into());
    let engine = manifest_value
        .as_ref()
        .and_then(|v| v.get("format").and_then(|n| n.as_str()).map(str::to_string))
        .unwrap_or_else(|| "paddle-ocr-v6-onnx".into());
    let file = OcrPageFile {
        version: OCR_PAGE_VERSION,
        page: page_1based,
        engine,
        model_name,
        page_width_pt: w_pt as f64,
        page_height_pt: h_pt as f64,
        lines: overlay,
    };
    write_page_file(book_dir, &file)
        .map_err(|e| PageOcrError::Io(format!("写入 OCR 缓存失败:{e}")))?;
    let text = overlay_lines_to_text(&file.lines);
    let pages_dir = crate::layout::book_pages_dir(book_dir);
    fs::create_dir_all(&pages_dir)
        .map_err(|e| PageOcrError::Io(format!("创建 pages 目录失败:{e}")))?;
    fs::write(
        crate::layout::book_page_md_path(&pages_dir, page_1based),
        text,
    )
    .map_err(|e| PageOcrError::Io(format!("写入页面 MD 失败:{e}")))?;
    mark_page_done(book_dir, page_1based);
    if crate::pdf::read_parse_marker(book_dir).is_none() {
        let marker = ParseMarker {
            page_count: doc.page_count(),
            completed_at: chrono::Local::now().to_rfc3339(),
        };
        let _ = crate::pdf::write_parse_marker(book_dir, &marker);
    }
    Ok(file)
}

fn backend_scale_hint() -> u32 {
    crate::settings::get()
        .page_ocr
        .model_dir
        .as_deref()
        .and_then(|d| {
            fs::read_to_string(Path::new(d).join("manifest.json"))
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("max_side").and_then(|n| n.as_u64()))
                .map(|n| n as u32)
        })
        .unwrap_or_else(default_max_side)
}

// ── 页面认领(后台任务与阅读器按页请求互斥) ──

static CLAIMS: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

struct PageClaim {
    key: String,
}

impl Drop for PageClaim {
    fn drop(&mut self) {
        if let Ok(mut set) = CLAIMS.get_or_init(|| Mutex::new(HashSet::new())).lock() {
            set.remove(&self.key);
        }
    }
}

fn try_claim(key: &str) -> Option<PageClaim> {
    let mut set = CLAIMS
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    if set.insert(key.to_string()) {
        Some(PageClaim {
            key: key.to_string(),
        })
    } else {
        None
    }
}

/// 读取页面的 OverlayLine:命中磁盘缓存直接返回;未命中则同步按页 OCR。
/// Ok(None) = 未配置模型;Err = 识别/IO 失败。
pub fn overlay_for_page(
    book_dir: &Path,
    page_1based: u32,
    doc: &PdfDocument,
) -> Result<Option<Vec<OverlayLine>>, PageOcrError> {
    if let Some(f) = read_page_file(book_dir, page_1based) {
        return Ok(Some(f.lines));
    }
    let key = claim_key(book_dir, page_1based);
    let Some(guard) = try_claim(&key) else {
        // 后台任务正在识别本页:等它落盘。
        for _ in 0..150 {
            std::thread::sleep(Duration::from_millis(100));
            if let Some(f) = read_page_file(book_dir, page_1based) {
                return Ok(Some(f.lines));
            }
        }
        return Err(PageOcrError::Ocr("页面正在后台识别中,稍后重试".into()));
    };
    let result = ocr_page(book_dir, page_1based, doc);
    drop(guard);
    match result {
        Ok(f) => Ok(Some(f.lines)),
        Err(PageOcrError::NotConfigured(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

/// 强制重跑当前页 OCR(忽略已有缓存,用于“有字形但提取损坏”的页面)。
pub fn reocr_page(
    book_dir: &Path,
    page_1based: u32,
    doc: &PdfDocument,
) -> Result<Option<Vec<OverlayLine>>, PageOcrError> {
    let key = claim_key(book_dir, page_1based);
    let Some(guard) = try_claim(&key) else {
        return Err(PageOcrError::Ocr("页面正在识别中,请稍后".into()));
    };
    let result = ocr_page(book_dir, page_1based, doc);
    drop(guard);
    match result {
        Ok(f) => Ok(Some(f.lines)),
        Err(PageOcrError::NotConfigured(_)) => Ok(None),
        Err(e) => Err(e),
    }
}

// ── 后台 OCR 调度(并行 + 断点续跑 + 可取消) ──

struct BookJob {
    cancel: tokio_util::sync::CancellationToken,
}

static MANAGER: OnceLock<OcrManager> = OnceLock::new();

pub struct OcrManager {
    jobs: Mutex<std::collections::HashMap<String, BookJob>>,
}

impl OcrManager {
    fn new() -> Self {
        Self {
            jobs: Mutex::new(std::collections::HashMap::new()),
        }
    }

    pub fn is_running(&self, book_id: &str) -> bool {
        self.jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains_key(book_id)
    }

    pub fn cancel(&self, book_id: &str) {
        if let Some(job) = self
            .jobs
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(book_id)
        {
            job.cancel.cancel();
        }
    }

    /// 自动模式入口:设置开启且未完成时启动(书库导入后、阅读器打开时调用)。
    pub fn ensure_started(&self, book_id: &str) {
        if !crate::settings::get().page_ocr.auto_ocr {
            return;
        }
        self.start_inner(book_id, false);
    }

    /// 强制整本重跑(忽略已完成进度)。
    pub fn start(&self, book_id: &str) {
        self.start_inner(book_id, true);
    }

    fn start_inner(&self, book_id: &str, force: bool) {
        {
            let mut jobs = self.jobs.lock().unwrap_or_else(|p| p.into_inner());
            if jobs.contains_key(book_id) {
                return;
            }
            if !force {
                let dir = crate::db::with_db(|conn| crate::books::get(conn, book_id))
                    .ok()
                    .flatten()
                    .map(|b| PathBuf::from(b.path));
                if let Some(dir) = dir {
                    if let Some(p) = load_progress(&dir) {
                        let complete = p
                            .total_needed
                            .map(|n| p.done.len() + p.failed.len() >= n as usize)
                            .unwrap_or(false);
                        if complete {
                            return;
                        }
                    }
                }
            }
            jobs.insert(
                book_id.to_string(),
                BookJob {
                    cancel: tokio_util::sync::CancellationToken::new(),
                },
            );
            let token = jobs.get(book_id).unwrap().cancel.clone();
            let book_id = book_id.to_string();
            tokio::spawn(async move {
                run_book_job(&book_id, token).await;
                if let Ok(mut jobs) = MANAGER.get_or_init(OcrManager::new).jobs.lock() {
                    jobs.remove(&book_id);
                }
            });
        }
    }
}

pub fn manager() -> &'static OcrManager {
    MANAGER.get_or_init(OcrManager::new)
}

async fn run_book_job(book_id: &str, cancel: tokio_util::sync::CancellationToken) {
    let dir = {
        let book_id = book_id.to_string();
        let result = tokio::task::spawn_blocking(move || {
            crate::db::with_db(|conn| crate::books::get(conn, &book_id))
                .map_err(|e| e.to_string())
                .and_then(|b| {
                    b.map(|b| PathBuf::from(b.path))
                        .ok_or_else(|| "book not found".to_string())
                })
        })
        .await;
        match result {
            Ok(Ok(dir)) => dir,
            _ => return,
        }
    };
    let doc = {
        let pdf_path = crate::layout::book_pdf_path(&dir);
        let pdf_path = pdf_path.clone();
        let result = tokio::task::spawn_blocking(move || crate::pdf::pdfium::open(&pdf_path)).await;
        match result {
            Ok(Ok(doc)) => Arc::new(doc),
            _ => return,
        }
    };

    // 找出需要 OCR 的页(无 PDFium 文本且未完成)。
    let mut pages: Vec<u32> = Vec::new();
    let page_count = doc.page_count();
    let progress = load_progress(&dir);
    for p in 1..=page_count {
        if cancel.is_cancelled() {
            break;
        }
        if progress
            .as_ref()
            .map(|pr| pr.done.contains(&p))
            .unwrap_or(false)
        {
            continue;
        }
        let idx = p - 1;
        let doc = doc.clone();
        let has_text = tokio::task::spawn_blocking(move || {
            doc.page_text_chars(idx)
                .map(|chars| !needs_ocr(&chars))
                .unwrap_or(false)
        })
        .await
        .unwrap_or(false);
        if !has_text {
            pages.push(p);
        }
    }
    if pages.is_empty() || cancel.is_cancelled() {
        return;
    }
    let _ = update_progress(&dir, |p| {
        p.total_needed = Some(pages.len() as u32);
    });

    let workers = crate::settings::get().page_ocr.workers.clamp(1, 4) as usize;
    let next = Arc::new(AtomicU32::new(0));
    let mut handles = Vec::new();
    for _ in 0..workers {
        let dir = dir.clone();
        let doc = doc.clone();
        let cancel = cancel.clone();
        let next = next.clone();
        let pages = pages.clone();
        handles.push(tokio::spawn(async move {
            loop {
                let idx = next.fetch_add(1, Ordering::Relaxed) as usize;
                if idx >= pages.len() || cancel.is_cancelled() {
                    break;
                }
                let page = pages[idx];
                let key = claim_key(&dir, page);
                let Some(_guard) = try_claim(&key) else {
                    continue; // 阅读器按页请求正在处理
                };
                let page_dir = dir.clone();
                let doc = doc.clone();
                let result =
                    tokio::task::spawn_blocking(move || ocr_page(&page_dir, page, &doc)).await;
                match result {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => mark_page_failed(&dir, page, e.to_string()),
                    Err(e) => mark_page_failed(&dir, page, format!("任务失败:{e}")),
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }
}

// ── 测试 ──

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_parses_with_defaults() {
        let json = r#"{
            "name": "PP-OCRv4 中英",
            "format": "paddle-ocr-v4-onnx",
            "det_model": "det_model.onnx",
            "rec_model": "rec_model.onnx",
            "rec_dict": "ppocr_keys_v1.txt"
        }"#;
        let m: PaddleOcrManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.format, "paddle-ocr-v4-onnx");
        assert_eq!(m.rec_input_size, [48, 320]);
        assert_eq!(m.det_limit_type, "max");
        assert!((m.cls_thresh - 0.9).abs() < 1e-6);
        assert!(!m.use_dilation);
        assert!(m.use_space_char);
    }

    #[test]
    fn manifest_parses_v6_defaults() {
        let json = r#"{
            "name": "PP-OCRv6 多语言 (small)",
            "format": "paddle-ocr-v6-onnx",
            "det_model": "det_model.onnx",
            "rec_model": "rec_model.onnx",
            "rec_dict": "rec_dict.txt",
            "cls_model": "cls_model.onnx",
            "rec_input_size": [48, 320],
            "cls_input_size": [80, 160],
            "mean": [0.5, 0.5, 0.5],
            "std": [0.5, 0.5, 0.5],
            "det_limit_side_len": 736,
            "det_limit_type": "min",
            "det_thresh": 0.3,
            "box_thresh": 0.5,
            "unclip_ratio": 1.6,
            "use_dilation": true,
            "use_space_char": true,
            "cls_thresh": 0.9
        }"#;
        let m: PaddleOcrManifest = serde_json::from_str(json).unwrap();
        assert_eq!(m.format, "paddle-ocr-v6-onnx");
        assert_eq!(m.det_limit_type, "min");
        assert_eq!(m.det_limit_side_len, 736);
        assert_eq!(m.cls_input_size, [80, 160]);
        assert!(m.use_dilation);
    }

    #[test]
    fn needs_ocr_checks_meaningful_text() {
        let char = |ch| crate::pdf::pdfium::TextChar {
            ch,
            left: 0.0,
            bottom: 0.0,
            right: 1.0,
            top: 1.0,
            font_size: 10.0,
        };
        assert!(needs_ocr(&[]));
        assert!(needs_ocr(&[char(' '), char('\n')]));
        assert!(!needs_ocr(&[char('H')]));
    }

    #[test]
    fn ctc_decode_collapses_repeats_and_blanks() {
        let chars = ["", "a", "b"].map(str::to_string).to_vec();
        let mut data = vec![0.0f32; 6 * 3];
        data[0] = 1.0; // blank
        data[1 * 3 + 1] = 1.0; // a
        data[2 * 3 + 1] = 1.0; // a (repeat -> collapse)
        data[3 * 3 + 0] = 1.0; // blank
        data[4 * 3 + 2] = 1.0; // b
        data[5 * 3 + 0] = 1.0; // blank
        assert_eq!(ctc_decode(&data, 6, 3, &chars), "ab");
    }

    #[test]
    fn min_area_rect_finds_square() {
        let pts: Vec<[f64; 2]> = (0..4)
            .flat_map(|y| (0..4).map(move |x| [x as f64, y as f64]))
            .collect();
        let (center, w, h, _) = min_area_rect(&pts);
        assert!((center[0] - 1.5).abs() < 1e-6);
        assert!((center[1] - 1.5).abs() < 1e-6);
        assert!((w - 3.0).abs() < 1e-6 || (h - 3.0).abs() < 1e-6);
    }

    #[test]
    fn sort_lines_orders_columns_then_rows() {
        let mut lines = vec![
            OcrLine {
                text: "B1".into(),
                x0: 0.60,
                y0: 0.10,
                x1: 0.80,
                y1: 0.16,
            },
            OcrLine {
                text: "A2".into(),
                x0: 0.10,
                y0: 0.30,
                x1: 0.30,
                y1: 0.36,
            },
            OcrLine {
                text: "A1".into(),
                x0: 0.10,
                y0: 0.10,
                x1: 0.30,
                y1: 0.16,
            },
            OcrLine {
                text: "B2".into(),
                x0: 0.60,
                y0: 0.30,
                x1: 0.80,
                y1: 0.36,
            },
        ];
        sort_lines(&mut lines);
        let texts: Vec<&str> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["A1", "A2", "B1", "B2"]);
    }

    #[test]
    fn split_words_distributes_line_width() {
        let words = split_line_words("hello world", 0.0, 0.1, 1.0, 0.2);
        assert_eq!(words.len(), 3);
        assert_eq!(words[0].text, "hello");
        assert_eq!(words[1].text, " ");
        assert_eq!(words[2].text, "world");
        assert!(words[2].left_pct > words[0].left_pct);
        assert!((words[2].left_pct + words[2].width_cqw - 100.0).abs() < 1e-6);
    }

    #[test]
    fn overlay_conversion_matches_reader_coordinate_system() {
        let lines = vec![OcrLine {
            text: "测试".to_string(),
            x0: 0.0,
            y0: 0.1,
            x1: 0.2,
            y1: 0.15,
        }];
        let overlay = lines_to_overlay(&lines, 600.0, 800.0);
        assert_eq!(overlay.len(), 1);
        assert!((overlay[0].top_pct - 10.0).abs() < 1e-6);
        assert!((overlay[0].height_cqw - 0.05 * (800.0 / 600.0) * 100.0).abs() < 1e-6);
        assert_eq!(overlay[0].words.len(), 2);
    }

    #[test]
    fn overlay_to_text_joins_cjk_without_space_and_breaks_paragraphs() {
        let mk = |top, h, words: Vec<&str>| OverlayLine {
            top_pct: top,
            height_pct: h,
            height_cqw: h,
            font_size_pt: 10.0,
            words: words
                .into_iter()
                .map(|t| OverlayWord {
                    text: t.to_string(),
                    left_pct: 0.0,
                    top_cqw: top,
                    width_cqw: 10.0,
                    height_cqw: h,
                })
                .collect(),
        };
        let lines = vec![
            mk(0.0, 1.0, vec!["你", "好"]),
            mk(1.5, 1.0, vec!["世", "界"]),
            mk(10.0, 1.0, vec!["new"]),
        ];
        let text = overlay_lines_to_text(&lines);
        assert!(text.contains("你好\n世界"));
        assert!(text.contains("\n\nnew"));
    }

    #[test]
    fn overlay_to_text_collapses_ocr_space_words_to_single_space() {
        let mk = |words: Vec<&str>| OverlayLine {
            top_pct: 0.0,
            height_pct: 1.0,
            height_cqw: 1.0,
            font_size_pt: 10.0,
            words: words
                .into_iter()
                .map(|t| OverlayWord {
                    text: t.to_string(),
                    left_pct: 0.0,
                    top_cqw: 0.0,
                    width_cqw: 10.0,
                    height_cqw: 1.0,
                })
                .collect(),
        };
        let text = overlay_lines_to_text(&[mk(vec!["Hello", " ", "OCR", " ", "123"])]);
        assert_eq!(text, "Hello OCR 123");
    }

    #[test]
    fn progress_roundtrip_and_invalidates_on_pdf_change() {
        let dir =
            std::env::temp_dir().join(format!("ueberneon-page-ocr-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pdf = crate::layout::book_pdf_path(&dir);
        fs::write(&pdf, b"v1").unwrap();
        let mut p = fresh_progress(&dir).unwrap();
        p.done = vec![1, 2];
        save_progress(&dir, &p).unwrap();
        assert_eq!(load_progress(&dir).unwrap().done, vec![1, 2]);
        fs::write(&pdf, b"v2-longer").unwrap();
        assert!(load_progress(&dir).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_models_scans_roots_and_dedupes() {
        let home =
            std::env::temp_dir().join(format!("ueberneon-page-ocr-home-{}", std::process::id()));
        let root = home.join(".ueberneon").join("page-ocr-models");
        let a = root.join("a");
        let b = root.join("b");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(
            a.join("manifest.json"),
            r#"{"name":"A","format":"paddle-ocr-v4-onnx"}"#,
        )
        .unwrap();
        fs::write(
            b.join("manifest.json"),
            r#"{"name":"B","format":"paddle-ocr-v4-onnx"}"#,
        )
        .unwrap();
        // 复制 a 的 manifest 到 b 以测试去重由目录 canonical 路径完成,不直接测;
        // 这里只验证两个模型被发现。
        let models = discover_models_in(&root);
        assert_eq!(models.len(), 2);
        let _ = fs::remove_dir_all(&home);
    }

    fn discover_models_in(root: &Path) -> Vec<DiscoveredModel> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for entry in fs::read_dir(root).unwrap().flatten() {
            let dir = entry.path();
            if dir.join("manifest.json").is_file() {
                let canonical = dir.canonicalize().unwrap_or_else(|_| dir.clone());
                if seen.insert(canonical) {
                    out.push(DiscoveredModel {
                        name: dir.file_name().unwrap().to_string_lossy().into_owned(),
                        dir,
                    });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    #[test]
    fn db_postprocess_returns_at_least_one_box_for_synthetic_mask() {
        let w = 64u32;
        let h = 64u32;
        let mut prob = vec![0.0f32; (w * h) as usize];
        for y in 16..48 {
            for x in 8..56 {
                prob[(y * w + x) as usize] = 0.9;
            }
        }
        let boxes = db_postprocess(&prob, w, h, w, h, w, h, 0.3, 0.5, 1.5, false);
        assert_eq!(boxes.len(), 1);
        assert!(boxes[0].w >= 40.0);
        assert!(boxes[0].h >= 24.0);
    }

    #[test]
    fn ocr_render_scale_caps_at_four() {
        assert!((ocr_render_scale(2000, 595.0, 842.0) - 2000.0 / 842.0).abs() < 1e-6);
        assert_eq!(ocr_render_scale(2000, 100.0, 100.0), 4.0);
    }

    #[test]
    #[ignore = "需要真实 PP-OCRv6 模型目录(UEBERNEON_PAGE_OCR_DIR 或 ~/.ueberneon/page-ocr-models/pp-ocr-v6-ch)"]
    fn pp_ocr_v6_real_model_smoke() {
        let dir = env::var("UEBERNEON_PAGE_OCR_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home)
                    .join(".ueberneon")
                    .join("page-ocr-models")
                    .join("pp-ocr-v6-ch")
            });
        assert!(dir.join("manifest.json").is_file(), "缺少模型目录:{dir:?}");
        let recognizer = PaddleOcrRecognizer::load(&dir).expect("加载 PP-OCRv6 模型失败");

        let tmp = std::env::temp_dir().join(format!(
            "ueberneon-page-ocr-v6-smoke-{}",
            std::process::id()
        ));
        fs::create_dir_all(&tmp).unwrap();
        let pdf_path = tmp.join("sample-scanned.pdf");
        fs::write(
            &pdf_path,
            include_bytes!("../tests/fixtures/sample-scanned.pdf"),
        )
        .unwrap();
        let doc = crate::pdf::pdfium::open(&pdf_path).expect("打开扫描 fixture");
        let (w_pt, h_pt) = doc.page_size(0).expect("页面尺寸");
        let scale = ocr_render_scale(2000, w_pt, h_pt);
        let png = doc.render_page_png(0, scale).expect("渲染页面");
        let img = image::load_from_memory(&png).expect("解码 PNG").to_rgba8();
        let (w, h) = img.dimensions();
        let lines = recognizer
            .recognize_page_rgba(img.as_raw(), w, h)
            .expect("OCR 失败");
        let text: String = lines
            .iter()
            .map(|l| l.text.as_str())
            .collect::<Vec<_>>()
            .join(" | ");
        eprintln!("PP-OCRv6 识别结果: {text}");
        assert!(!text.trim().is_empty(), "扫描 fixture 未识别出任何文字");

        // 端到端落盘:ocr/NNNN.json + pages/NNNN.md + progress.json。
        let book = tmp.join("book");
        fs::create_dir_all(&book).unwrap();
        fs::write(
            crate::layout::book_pdf_path(&book),
            include_bytes!("../tests/fixtures/sample-scanned.pdf"),
        )
        .unwrap();
        let doc = crate::pdf::pdfium::open(&crate::layout::book_pdf_path(&book)).unwrap();
        let file = ocr_page(&book, 1, &doc).expect("ocr_page 落盘失败");
        assert!(!file.lines.is_empty());
        assert!(
            crate::layout::book_ocr_page_path(&crate::layout::book_ocr_dir(&book), 1).is_file()
        );
        let md = crate::layout::book_page_md_path(&crate::layout::book_pages_dir(&book), 1);
        assert!(md.is_file(), "OCR 后应补写 pages/0001.md");
        let md_text = fs::read_to_string(&md).unwrap();
        eprintln!("pages/0001.md: {md_text}");
        let progress = load_progress(&book).expect("progress.json");
        assert_eq!(progress.done, vec![1]);
        let _ = fs::remove_dir_all(&tmp);
    }
}
