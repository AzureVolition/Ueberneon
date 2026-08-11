// ── 公式 OCR 后端 ──
//
// 本地 ONNX Runtime(PP-FormulaNet_plus-S)识别公式选区并输出 LaTeX。
// 模型与运行库由 build.rs 嵌入;若构建时未提供资源,`backend()` 返回
// NotConfigured,上层回退到文本层重建。后端通过 trait 暴露,未来可替换
// 为 UniMERNet / Mathpix 等实现。

use std::collections::HashSet;
use std::env;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

// build.rs 生成的嵌入资源(可能为空切片,表示未配置)。
include!(concat!(env!("OUT_DIR"), "/bundled_formula.rs"));

const FORMULA_CACHE_VERSION: &str = "formula-ocr-v1";

#[derive(Debug, Clone)]
pub enum FormulaOcrError {
    NotConfigured(String),
    Io(String),
    Json(String),
    Ort(String),
    Decode(String),
}

impl std::fmt::Display for FormulaOcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormulaOcrError::NotConfigured(m) => write!(f, "{m}"),
            FormulaOcrError::Io(m) => write!(f, "IO 错误:{m}"),
            FormulaOcrError::Json(m) => write!(f, "配置解析错误:{m}"),
            FormulaOcrError::Ort(m) => write!(f, "ONNX Runtime 错误:{m}"),
            FormulaOcrError::Decode(m) => write!(f, "解码错误:{m}"),
        }
    }
}

impl std::error::Error for FormulaOcrError {}

/// 公式识别后端接口(通用,便于替换模型)。
pub trait FormulaRecognizer: Send + Sync {
    /// 输入 RGBA 图像(白底裁剪区),返回 LaTeX 源码。
    fn recognize_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<String, FormulaOcrError>;
}

/// 单槽缓存:只保留最近一次 (key, value);相同 key 直接复用,不同 key 覆盖。
#[derive(Debug, Clone, Default)]
pub struct SingleSlotCache {
    entry: Option<(String, String)>,
}

impl SingleSlotCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 仅当 key 与缓存完全一致时返回缓存值(不新增、不替换)。
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entry
            .as_ref()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// 写入(或覆盖)最近一次结果。
    pub fn put(&mut self, key: String, value: String) {
        self.entry = Some((key, value));
    }

    pub fn clear(&mut self) {
        self.entry = None;
    }
}

struct BackendSlot {
    key: String,
    backend: Result<std::sync::Arc<dyn FormulaRecognizer>, FormulaOcrError>,
}

static BACKEND_STATE: OnceLock<Mutex<Option<BackendSlot>>> = OnceLock::new();

/// 后端缓存键:配置的模型目录变化时自动重新加载,换模型无需重启。
fn backend_key() -> String {
    match configured_model_dir() {
        Some(dir) => format!("dir:{}", dir.display()),
        None if !ONNXRUNTIME_DYLIB.is_empty()
            && !FORMULA_MODEL.is_empty()
            && !FORMULA_DICT.is_empty() =>
        {
            "embedded".into()
        }
        None => "none".into(),
    }
}

/// 获取全局公式识别后端;配置的模型目录改变后会自动重新初始化。
pub fn backend_arc() -> Result<std::sync::Arc<dyn FormulaRecognizer>, FormulaOcrError> {
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
        Some(dir) if dir.join("manifest.json").is_file() => UniMernetRecognizer::load(&dir)
            .map(|b| std::sync::Arc::new(b) as std::sync::Arc<dyn FormulaRecognizer>),
        _ => OrtFormulaRecognizer::load()
            .map(|b| std::sync::Arc::new(b) as std::sync::Arc<dyn FormulaRecognizer>),
    };
    *state = Some(BackendSlot { key, backend });
    match state.as_ref().unwrap().backend.as_ref() {
        Ok(b) => Ok(b.clone()),
        Err(e) => Err(e.clone()),
    }
}

/// 预处理参数(由 scripts/export_formula_onnx.py 导出并固化)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PreprocessConfig {
    /// 等比缩放后的目标高度(像素)
    pub height: u32,
    /// RGB 归一化均值
    pub mean: [f32; 3],
    /// RGB 归一化标准差
    pub std: [f32; 3],
}

impl Default for PreprocessConfig {
    fn default() -> Self {
        Self {
            height: 48,
            mean: [0.485, 0.456, 0.406],
            std: [0.229, 0.224, 0.225],
        }
    }
}

pub struct OrtFormulaRecognizer {
    session: Mutex<ort::session::Session>,
    dict: Vec<String>,
    config: PreprocessConfig,
}

impl OrtFormulaRecognizer {
    pub fn load() -> Result<Self, FormulaOcrError> {
        let lib_path = ensure_asset("libonnxruntime.dylib", &ONNXRUNTIME_DYLIB)?;
        ort::init_from(&lib_path)
            .map_err(|e| FormulaOcrError::Ort(format!("init_from({lib_path:?}): {e}")))?
            .commit();
        let model_path = ensure_asset("model.onnx", &FORMULA_MODEL)?;
        let dict_bytes = asset_bytes("dict.json", &FORMULA_DICT)?;
        let dict: Vec<String> = serde_json::from_slice(&dict_bytes)
            .map_err(|e| FormulaOcrError::Json(format!("词表:{e}")))?;
        let preprocess_bytes = asset_bytes("preprocess.json", &FORMULA_PREPROCESS)?;
        let config: PreprocessConfig = serde_json::from_slice(&preprocess_bytes)
            .map_err(|e| FormulaOcrError::Json(format!("预处理参数:{e}")))?;
        let session = ort::session::Session::builder()
            .map_err(|e| FormulaOcrError::Ort(format!("builder:{e}")))?
            .with_intra_threads(4)
            .map_err(|e| FormulaOcrError::Ort(format!("intra_threads:{e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| FormulaOcrError::Ort(format!("commit_from_file:{e}")))?;
        Ok(Self {
            session: Mutex::new(session),
            dict,
            config,
        })
    }
}

impl FormulaRecognizer for OrtFormulaRecognizer {
    fn recognize_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<String, FormulaOcrError> {
        let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| FormulaOcrError::Decode("RGBA 尺寸与缓冲区不一致".into()))?;
        if img.width() == 0 || img.height() == 0 {
            return Err(FormulaOcrError::Decode("空图像".into()));
        }

        // 等比缩放到目标高度(模型动态宽度)。
        let target_h = self.config.height.max(1);
        let nw =
            (((img.width() as f32 * target_h as f32) / img.height() as f32).round() as u32).max(1);
        let resized =
            image::imageops::resize(&img, nw, target_h, image::imageops::FilterType::Triangle);

        // NCHW float 归一化。
        let (mean, std) = (self.config.mean, self.config.std);
        let mut input = ndarray::Array4::<f32>::zeros((1, 3, target_h as usize, nw as usize));
        for (x, y, px) in resized.enumerate_pixels() {
            let r = px.0[0] as f32 / 255.0;
            let g = px.0[1] as f32 / 255.0;
            let b = px.0[2] as f32 / 255.0;
            input[[0, 0, y as usize, x as usize]] = (r - mean[0]) / std[0];
            input[[0, 1, y as usize, x as usize]] = (g - mean[1]) / std[1];
            input[[0, 2, y as usize, x as usize]] = (b - mean[2]) / std[2];
        }

        let tensor = ort::value::TensorRef::from_array_view(&input)
            .map_err(|e| FormulaOcrError::Ort(format!("tensor:{e}")))?;
        let mut guard = self
            .session
            .lock()
            .map_err(|_| FormulaOcrError::Ort("session mutex poisoned".into()))?;
        let outputs = guard
            .run(ort::inputs![tensor])
            .map_err(|e| FormulaOcrError::Ort(format!("run:{e}")))?;
        let output = outputs
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| FormulaOcrError::Ort("模型无输出".into()))?;
        let tensor = output
            .downcast::<ort::value::TensorValueType<f32>>()
            .map_err(|e| FormulaOcrError::Ort(format!("downcast:{e}")))?;
        let logits = tensor
            .try_extract_array::<f32>()
            .map_err(|e| FormulaOcrError::Ort(format!("output:{e}")))?;
        let ids = greedy_argmax(&logits);
        Ok(decode_ids(&ids, &self.dict))
    }
}

/// UniMERNet 风格模型 manifest(由 scripts/export_unimernet_onnx.py 生成)。
#[derive(Debug, Clone, Deserialize)]
struct UniMernetManifest {
    format: String,
    input_size: [u32; 2],
    mean: [f32; 3],
    std: [f32; 3],
    output: String,
    tokenizer_file: String,
    special_tokens: Vec<String>,
}

/// UniMERNet / Nougat 风格后端:ONNX 内部自带自回归 Loop,
/// 一次 run 返回 token id 序列,这里只做 384×384 预处理 + BPE 解码。
pub struct UniMernetRecognizer {
    session: Mutex<ort::session::Session>,
    tokens: Vec<String>,
    special: HashSet<String>,
    input_size: [u32; 2],
    mean: [f32; 3],
    std: [f32; 3],
}

/// 扫描到的本地公式识别模型(manifest 驱动的桌面端发现)。
#[derive(Debug, Clone)]
pub struct DiscoveredModel {
    pub name: String,
    pub dir: PathBuf,
}

/// 扫描本地模型目录:
/// - `~/.ueberneon/formula-models/*/`(主目录,拖入即用)
/// - `$CARGO_HOME/ueberneon-formula/*/`(导出脚本缓存,兼容)
/// - `UEBERNEON_FORMULA_DIR`(若设置)
/// 子目录必须包含 `manifest.json` 才被识别,按名称排序、按真实路径去重。
pub fn discover_models() -> Vec<DiscoveredModel> {
    let mut roots = Vec::new();
    if let Ok(home) = env::var("HOME") {
        roots.push(
            PathBuf::from(home)
                .join(".ueberneon")
                .join("formula-models"),
        );
    }
    let cargo_home = env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
            PathBuf::from(home).join(".cargo")
        });
    roots.push(cargo_home.join("ueberneon-formula"));
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
    // UEBERNEON_FORMULA_DIR 可能直接指向模型目录本身(而非父目录)。
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

impl UniMernetRecognizer {
    pub fn load(dir: &Path) -> Result<Self, FormulaOcrError> {
        let manifest_bytes = fs::read(dir.join("manifest.json"))
            .map_err(|e| FormulaOcrError::Io(format!("读取 manifest.json:{e}")))?;
        let manifest: UniMernetManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| FormulaOcrError::Json(format!("manifest:{e}")))?;
        if manifest.format != "unimernet-onnx" {
            return Err(FormulaOcrError::NotConfigured(format!(
                "不支持的模型格式:{}",
                manifest.format
            )));
        }
        if manifest.output != "token_ids" {
            return Err(FormulaOcrError::NotConfigured(format!(
                "不支持的模型输出类型:{}",
                manifest.output
            )));
        }

        let lib_path = dir.join("libonnxruntime.dylib");
        if !lib_path.is_file() {
            return Err(FormulaOcrError::NotConfigured(format!(
                "缺少 {}",
                lib_path.display()
            )));
        }
        ort::init_from(&lib_path)
            .map_err(|e| FormulaOcrError::Ort(format!("init_from({lib_path:?}): {e}")))?
            .commit();

        let model_path = dir.join("model.onnx");
        let session = ort::session::Session::builder()
            .map_err(|e| FormulaOcrError::Ort(format!("builder:{e}")))?
            .with_intra_threads(4)
            .map_err(|e| FormulaOcrError::Ort(format!("intra_threads:{e}")))?
            .commit_from_file(&model_path)
            .map_err(|e| FormulaOcrError::Ort(format!("commit_from_file({model_path:?}):{e}")))?;

        let tokenizer_path = dir.join(&manifest.tokenizer_file);
        let raw: Vec<serde_json::Value> = {
            let bytes = fs::read(&tokenizer_path).map_err(|e| {
                FormulaOcrError::Io(format!("读取 {}:{e}", tokenizer_path.display()))
            })?;
            serde_json::from_slice(&bytes)
                .map_err(|e| FormulaOcrError::Json(format!("tokenizer:{e}")))?
        };
        let tokens = raw
            .into_iter()
            .map(|v| v.as_str().unwrap_or("").to_string())
            .collect::<Vec<_>>();

        Ok(Self {
            session: Mutex::new(session),
            tokens,
            special: manifest.special_tokens.into_iter().collect(),
            input_size: manifest.input_size,
            mean: manifest.mean,
            std: manifest.std,
        })
    }
}

impl FormulaRecognizer for UniMernetRecognizer {
    fn recognize_rgba(
        &self,
        rgba: &[u8],
        width: u32,
        height: u32,
    ) -> Result<String, FormulaOcrError> {
        let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| FormulaOcrError::Decode("RGBA 尺寸与缓冲区不一致".into()))?;
        if img.width() == 0 || img.height() == 0 {
            return Err(FormulaOcrError::Decode("空图像".into()));
        }
        let gray = image::DynamicImage::ImageRgba8(img).to_luma8();
        let cropped = crop_formula_margin(&gray);
        let input = unimernet_preprocess(&cropped, self.input_size, self.mean, self.std)?;

        let tensor = ort::value::TensorRef::from_array_view(&input)
            .map_err(|e| FormulaOcrError::Ort(format!("tensor:{e}")))?;
        let mut guard = self
            .session
            .lock()
            .map_err(|_| FormulaOcrError::Ort("session mutex poisoned".into()))?;
        let outputs = guard
            .run(ort::inputs![tensor])
            .map_err(|e| FormulaOcrError::Ort(format!("run:{e}")))?;
        let output = outputs
            .into_iter()
            .next()
            .map(|(_, v)| v)
            .ok_or_else(|| FormulaOcrError::Ort("模型无输出".into()))?;
        let tensor = output
            .downcast::<ort::value::TensorValueType<i64>>()
            .map_err(|e| FormulaOcrError::Ort(format!("downcast:{e}")))?;
        let arr = tensor
            .try_extract_array::<i64>()
            .map_err(|e| FormulaOcrError::Ort(format!("output:{e}")))?;
        let ids = arr.iter().copied().collect::<Vec<_>>();
        Ok(decode_bpe_ids(&ids, &self.tokens, &self.special))
    }
}

/// UniMERNet 预处理:去白边 → 等比缩放进 384×384 → 居中补黑边 → 归一化。
fn unimernet_preprocess(
    img: &image::GrayImage,
    input_size: [u32; 2],
    mean: [f32; 3],
    std: [f32; 3],
) -> Result<ndarray::Array4<f32>, FormulaOcrError> {
    let (w, h) = (img.width(), img.height());
    if w == 0 || h == 0 {
        return Err(FormulaOcrError::Decode("空图像".into()));
    }
    let scale = (input_size[1] as f32 / w as f32).min(input_size[0] as f32 / h as f32);
    let nw = ((w as f32 * scale).round() as u32).max(1);
    let nh = ((h as f32 * scale).round() as u32).max(1);
    let resized = image::imageops::resize(img, nw, nh, image::imageops::FilterType::Triangle);

    let mut canvas = image::GrayImage::from_pixel(input_size[1], input_size[0], image::Luma([0]));
    let ox = ((input_size[1] - nw) / 2) as i64;
    let oy = ((input_size[0] - nh) / 2) as i64;
    image::imageops::overlay(&mut canvas, &resized, ox, oy);

    let mut input =
        ndarray::Array4::<f32>::zeros((1, 1, input_size[0] as usize, input_size[1] as usize));
    for (x, y, px) in canvas.enumerate_pixels() {
        let v = px.0[0] as f32 / 255.0;
        input[[0, 0, y as usize, x as usize]] = (v - mean[0]) / std[0];
    }
    Ok(input)
}

/// 与 PaddleOCR 一致的去白边:对比度归一化后,<200 视为文字,
/// 取所有文字像素的外接矩形;极端宽高比或空图直接返回原图。
fn crop_formula_margin(img: &image::GrayImage) -> image::GrayImage {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return img.clone();
    }
    let mut min = 255u8;
    let mut max = 0u8;
    for p in img.pixels() {
        min = min.min(p.0[0]);
        max = max.max(p.0[0]);
    }
    if max == min {
        return img.clone();
    }
    let mut left = w;
    let mut right = 0u32;
    let mut top = h;
    let mut bottom = 0u32;
    for (x, y, p) in img.enumerate_pixels() {
        let norm = ((p.0[0] as u32 - min as u32) * 255 / (max - min) as u32) as u8;
        if norm < 200 {
            left = left.min(x);
            right = right.max(x);
            top = top.min(y);
            bottom = bottom.max(y);
        }
    }
    if right < left || bottom < top {
        return img.clone();
    }
    let bw = (right - left + 1) as f64;
    let bh = (bottom - top + 1) as f64;
    if bw.max(bh) / bw.min(bh).max(1.0) > 200.0 {
        return img.clone();
    }
    image::imageops::crop_imm(img, left, top, bw as u32, bh as u32).to_image()
}

/// BPE(ByteLevel)解码:跳过特殊 token,`Ġ`→空格、`Ċ`→换行,
/// `<0xXX>` 还原为字节字符。
fn decode_bpe_ids(ids: &[i64], tokens: &[String], special: &HashSet<String>) -> String {
    let mut out = String::new();
    for &id in ids {
        let Some(tok) = tokens.get(id as usize) else {
            continue;
        };
        if tok.is_empty() || special.contains(tok.as_str()) {
            continue;
        }
        // PP-FormulaNet_plus-S / UniMERNet 的并行解码把 `Ġ` 当作
        // token 分隔符输出,而不是真正的空格;单独的 `Ġ` 应跳过。
        if tok == "Ġ" {
            continue;
        }
        if let Some(hex) = tok.strip_prefix("<0x").and_then(|s| s.strip_suffix('>')) {
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b as char);
                continue;
            }
        }
        for c in tok.chars() {
            match c {
                'Ġ' => out.push(' '),
                'Ċ' => out.push('\n'),
                _ => out.push(c),
            }
        }
    }
    out.trim().to_string()
}

/// 按最后一维贪心解码;支持 [1, T, V] 或 [T, V] 输出。
fn greedy_argmax(logits: &ndarray::ArrayViewD<f32>) -> Vec<usize> {
    let shape = logits.shape();
    let mut ids = Vec::new();
    if shape.len() == 3 {
        if let Ok(v) = logits.to_owned().into_dimensionality::<ndarray::Ix3>() {
            let (_, seq, vocab) = v.dim();
            for t in 0..seq {
                let slice = v.slice(ndarray::s![0, t, ..]);
                if let Some((i, _)) = argmax(&slice, vocab) {
                    ids.push(i);
                }
            }
        }
    } else if shape.len() == 2 {
        if let Ok(v) = logits.to_owned().into_dimensionality::<ndarray::Ix2>() {
            let (seq, vocab) = v.dim();
            for t in 0..seq {
                let slice = v.slice(ndarray::s![t, ..]);
                if let Some((i, _)) = argmax(&slice, vocab) {
                    ids.push(i);
                }
            }
        }
    }
    ids
}

fn argmax(slice: &ndarray::ArrayView1<f32>, len: usize) -> Option<(usize, f32)> {
    if len == 0 {
        return None;
    }
    slice
        .iter()
        .enumerate()
        .max_by(|a, b| a.1.partial_cmp(b.1).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(i, v)| (i, *v))
}

/// 把词表 id 序列解码为 LaTeX;遇到 EOS 停止,跳过空白/blank 标记。
fn decode_ids(ids: &[usize], dict: &[String]) -> String {
    let mut out = String::new();
    for &id in ids {
        let Some(tok) = dict.get(id) else {
            continue;
        };
        let t = tok.trim();
        if t.is_empty() || t == "blank" {
            continue;
        }
        if matches!(t, "<eos>" | "</s>" | "[EOS]" | "<pad>" | "[PAD]") {
            break;
        }
        out.push_str(tok);
    }
    out.trim().to_string()
}

/// 模型目录来源:设置里的 model_dir 优先,其次 UEBERNEON_FORMULA_DIR 环境变量。
fn configured_model_dir() -> Option<PathBuf> {
    if let Some(dir) = crate::settings::get().formula_ocr.model_dir {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    if let Ok(dir) = env::var("UEBERNEON_FORMULA_DIR") {
        let p = PathBuf::from(dir);
        if p.is_dir() {
            return Some(p);
        }
    }
    None
}

/// 读取资源:配置目录优先,其次构建期嵌入字节。
fn asset_bytes(name: &str, embedded: &[u8]) -> Result<Vec<u8>, FormulaOcrError> {
    if let Some(dir) = configured_model_dir() {
        let p = dir.join(name);
        if p.is_file() {
            return fs::read(&p).map_err(|e| FormulaOcrError::Io(format!("读取 {p:?}:{e}")));
        }
    }
    if !embedded.is_empty() {
        return Ok(embedded.to_vec());
    }
    Err(FormulaOcrError::NotConfigured(format!(
        "缺少 {name};请在设置的“公式识别”中选择模型目录,或运行 scripts/export_formula_onnx.py 后重新构建"
    )))
}

/// 把嵌入/本地资源解压到缓存目录(临时文件 + rename,并发安全)。
fn ensure_asset(name: &str, bytes: &[u8]) -> Result<PathBuf, FormulaOcrError> {
    if let Some(dir) = configured_model_dir() {
        let p = dir.join(name);
        if p.is_file() {
            return Ok(p);
        }
    }
    if bytes.is_empty() {
        return Err(FormulaOcrError::NotConfigured(format!(
            "缺少 {name};请在设置的“公式识别”中选择模型目录,或运行 scripts/export_formula_onnx.py 后重新构建"
        )));
    }
    let cache_root = cache_root();
    fs::create_dir_all(&cache_root)
        .map_err(|e| FormulaOcrError::Io(format!("创建缓存目录 {}:{e}", cache_root.display())))?;
    let dest = cache_root.join(name);
    if dest.is_file() {
        if let Ok(meta) = fs::metadata(&dest) {
            if meta.len() == bytes.len() as u64 {
                return Ok(dest);
            }
        }
    }
    let tmp = cache_root.join(format!("{name}.tmp-{}", std::process::id()));
    let mut f = fs::File::create(&tmp)
        .map_err(|e| FormulaOcrError::Io(format!("创建临时文件 {tmp:?}:{e}")))?;
    f.write_all(bytes)
        .map_err(|e| FormulaOcrError::Io(format!("写入 {tmp:?}:{e}")))?;
    f.flush()
        .map_err(|e| FormulaOcrError::Io(format!("flush {tmp:?}:{e}")))?;
    drop(f);
    if dest.exists() {
        fs::remove_file(&dest)
            .map_err(|e| FormulaOcrError::Io(format!("清理旧文件 {dest:?}:{e}")))?;
    }
    fs::rename(&tmp, &dest)
        .map_err(|e| FormulaOcrError::Io(format!("rename {tmp:?} -> {dest:?}:{e}")))?;
    Ok(dest)
}

fn cache_root() -> PathBuf {
    if let Ok(dir) = env::var("UEBERNEON_CACHE_DIR") {
        return PathBuf::from(dir).join(FORMULA_CACHE_VERSION);
    }
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home)
        .join("Library/Caches/ueberneon")
        .join(FORMULA_CACHE_VERSION)
}

/// 文本层重建的兜底映射:Unicode 数学字符 → LaTeX 命令。
pub fn latex_escape_unicode(text: &str) -> String {
    const MAP: &[(char, &str)] = &[
        ('α', "\\alpha"),
        ('β', "\\beta"),
        ('γ', "\\gamma"),
        ('δ', "\\delta"),
        ('ε', "\\epsilon"),
        ('ζ', "\\zeta"),
        ('η', "\\eta"),
        ('θ', "\\theta"),
        ('ι', "\\iota"),
        ('κ', "\\kappa"),
        ('λ', "\\lambda"),
        ('μ', "\\mu"),
        ('ν', "\\nu"),
        ('ξ', "\\xi"),
        ('π', "\\pi"),
        ('ρ', "\\rho"),
        ('σ', "\\sigma"),
        ('τ', "\\tau"),
        ('υ', "\\upsilon"),
        ('φ', "\\phi"),
        ('χ', "\\chi"),
        ('ψ', "\\psi"),
        ('ω', "\\omega"),
        ('Γ', "\\Gamma"),
        ('Δ', "\\Delta"),
        ('Θ', "\\Theta"),
        ('Λ', "\\Lambda"),
        ('Ξ', "\\Xi"),
        ('Π', "\\Pi"),
        ('Σ', "\\Sigma"),
        ('Υ', "\\Upsilon"),
        ('Φ', "\\Phi"),
        ('Ψ', "\\Psi"),
        ('Ω', "\\Omega"),
        ('∗', "*"),
        ('×', "\\times"),
        ('÷', "\\div"),
        ('±', "\\pm"),
        ('≤', "\\le"),
        ('≥', "\\ge"),
        ('≠', "\\ne"),
        ('≈', "\\approx"),
        ('≡', "\\equiv"),
        ('∞', "\\infty"),
        ('∑', "\\sum"),
        ('∏', "\\prod"),
        ('∫', "\\int"),
        ('∂', "\\partial"),
        ('∈', "\\in"),
        ('∉', "\\notin"),
        ('→', "\\to"),
        ('←', "\\leftarrow"),
        ('⇒', "\\Rightarrow"),
        ('⇔', "\\Leftrightarrow"),
        ('∀', "\\forall"),
        ('∃', "\\exists"),
        ('∪', "\\cup"),
        ('∩', "\\cap"),
        ('⊆', "\\subseteq"),
        ('⊂', "\\subset"),
        ('⊇', "\\supseteq"),
        ('⊃', "\\supset"),
        ('∧', "\\wedge"),
        ('∨', "\\vee"),
        ('¬', "\\neg"),
        ('∣', "\\mid"),
        ('∥', "\\parallel"),
        ('⋅', "\\cdot"),
        ('·', "\\cdot"),
        ('…', "\\dots"),
    ];
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if let Some((_, latex)) = MAP.iter().find(|(ch, _)| *ch == c) {
            out.push_str(latex);
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latex_escape_unicode_maps_math_symbols() {
        assert_eq!(latex_escape_unicode("Θ=π/2"), "\\Theta=\\pi/2");
        assert_eq!(latex_escape_unicode("p∗"), "p*");
        assert_eq!(latex_escape_unicode("普通文本"), "普通文本");
    }

    #[test]
    fn decode_ids_skips_blank_and_stops_at_eos() {
        let dict = vec![
            "".into(),
            "\\frac".into(),
            "{".into(),
            "<eos>".into(),
            "x".into(),
        ];
        assert_eq!(decode_ids(&[1, 2, 4, 0, 3, 1], &dict), "\\frac{x");
    }

    #[test]
    fn greedy_argmax_handles_2d_and_3d() {
        let arr =
            ndarray::Array2::<f32>::from_shape_vec((2, 3), vec![0.1, 0.9, 0.2, 0.8, 0.1, 0.1])
                .unwrap();
        let ids = greedy_argmax(&arr.view().into_dyn());
        assert_eq!(ids, vec![1, 0]);
    }

    #[test]
    fn unimernet_manifest_parses() {
        let json = r#"{
            "format": "unimernet-onnx",
            "input_size": [384, 384],
            "mean": [0.7931, 0.7931, 0.7931],
            "std": [0.1738, 0.1738, 0.1738],
            "output": "token_ids",
            "tokenizer_file": "tokenizer.json",
            "special_tokens": ["<s>", "</s>"]
        }"#;
        let manifest: UniMernetManifest = serde_json::from_str(json).unwrap();
        assert_eq!(manifest.input_size, [384, 384]);
        assert_eq!(manifest.special_tokens, vec!["<s>", "</s>"]);
    }

    #[test]
    fn decode_bpe_ids_handles_byte_level_and_special() {
        let tokens = vec![
            "<s>".to_string(),
            "Ġx".to_string(),
            "\\frac".to_string(),
            "<0xCE>".to_string(),
            "Ċ".to_string(),
            "</s>".to_string(),
            "Ġ".to_string(),
        ];
        let special: HashSet<String> = ["<s>", "</s>"].iter().map(|s| s.to_string()).collect();
        assert_eq!(
            decode_bpe_ids(&[0, 1, 2, 6, 3, 6, 4, 1, 5], &tokens, &special),
            "x\\fracÎ\n x"
        );
    }

    #[test]
    fn crop_formula_margin_trims_white_border() {
        let mut img = image::GrayImage::from_pixel(100, 100, image::Luma([255]));
        for y in 30..=50 {
            for x in 20..=60 {
                img.put_pixel(x, y, image::Luma([0]));
            }
        }
        let cropped = crop_formula_margin(&img);
        assert_eq!(cropped.dimensions(), (41, 21));
    }

    #[test]
    fn unimernet_preprocess_builds_384_channel_first_tensor() {
        let img = image::GrayImage::from_pixel(192, 96, image::Luma([128]));
        let input = unimernet_preprocess(&img, [384, 384], [0.7931; 3], [0.1738; 3]).unwrap();
        assert_eq!(input.shape(), &[1, 1, 384, 384]);
    }

    #[test]
    #[ignore = "需要真实模型目录(UEBERNEON_FORMULA_DIR 或 ~/.cargo/ueberneon-formula/unimernet)"]
    fn unimernet_real_model_roundtrip() {
        let dir = env::var("UEBERNEON_FORMULA_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| {
                let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                PathBuf::from(home).join(".cargo/ueberneon-formula/unimernet")
            });
        let recognizer = UniMernetRecognizer::load(&dir).expect("加载 UniMERNet 模型失败");
        let (w, h, rgba) = if let Ok(path) = env::var("UEBERNEON_TEST_IMAGE") {
            let img = image::open(&path).expect("打开测试图片").to_rgba8();
            let (w, h) = img.dimensions();
            (w, h, img.into_raw())
        } else {
            let (w, h) = (200u32, 60u32);
            let mut rgba = vec![255u8; (w * h * 4) as usize];
            for y in 10..50 {
                for x in 20..180 {
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i] = 0;
                    rgba[i + 1] = 0;
                    rgba[i + 2] = 0;
                }
            }
            (w, h, rgba)
        };
        let latex = recognizer.recognize_rgba(&rgba, w, h).expect("识别失败");
        eprintln!("unimernet latex={latex}");
        assert!(!latex.is_empty());
    }

    #[test]
    fn single_slot_cache_keeps_only_latest() {
        let mut cache = SingleSlotCache::new();
        assert_eq!(cache.get("a"), None);
        cache.put("a".into(), "A".into());
        assert_eq!(cache.get("a"), Some("A"));
        // 相同 key 重复读取不新增条目
        assert_eq!(cache.get("a"), Some("A"));
        cache.put("b".into(), "B".into());
        assert_eq!(cache.get("a"), None, "不同 key 覆盖旧结果");
        assert_eq!(cache.get("b"), Some("B"));
        cache.clear();
        assert_eq!(cache.get("b"), None);
    }
}
