// ── 自研 PDFium FFI 封装层 ──
//
// 不依赖 pdfium-render(未到 1.0),只封装阅读器与文本提取需要的 API 子集:
//   FPDF_InitLibrary / FPDF_LoadDocument / FPDF_GetPageCount
//   FPDF_GetPageWidth / FPDF_GetPageHeight / FPDF_LoadPage / FPDF_ClosePage
//   FPDFBitmap_CreateEx / FPDFBitmap_Destroy / FPDF_RenderPageBitmap
//   FPDF_CloseDocument / FPDFText_LoadPage / FPDFText_CountChars
//   FPDFText_GetUnicode / FPDFText_ClosePage
//
// PDFium 动态库在构建期由 build.rs 嵌入可执行文件,运行时首次解压到
// ~/Library/Caches/ueberneon/pdfium-7961/ 后经 libloading 加载;
// 也可用 PDFIUM_LIB_PATH 环境变量直接指定 dylib 路径。
//
// PDFium 不是线程安全的,所有调用通过全局 Mutex 串行化。

use std::env;
use std::ffi::CString;
use std::fmt;
use std::fs;
use std::os::raw::{c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use libloading::{Library, Symbol};

use crate::layout;

const PDFIUM_VERSION: &str = "7961";
const MIN_LIB_BYTES: u64 = 1_000_000;

/// 构建期由 build.rs 放到 OUT_DIR 的 PDFium 动态库字节。
const EMBEDDED_PDFIUM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/libpdfium.dylib"));

// ── 不透明句柄 ──

#[allow(non_camel_case_types)]
type FPDF_DOCUMENT = *mut c_void;
#[allow(non_camel_case_types)]
type FPDF_PAGE = *mut c_void;
#[allow(non_camel_case_types)]
type FPDF_BITMAP = *mut c_void;
#[allow(non_camel_case_types)]
type FPDF_TEXTPAGE = *mut c_void;

/// FPDFBitmap_BGRx:4 字节/像素,无 alpha。
/// 注意:PDFium chromium/7961 起该值从 0 改为 3(0 = FPDFBitmap_Unknown)。
const FPDF_BITMAP_BGRX: c_int = 3;

// ── 错误类型 ──

#[derive(Debug)]
pub enum PdfiumError {
    /// 动态库加载/初始化失败
    Init(String),
    /// 文档打开失败
    Load { path: PathBuf, reason: String },
    /// 页面渲染失败
    Render(String),
    /// 文本提取失败
    Text(String),
    /// 文件系统错误
    Io(std::io::Error),
}

impl fmt::Display for PdfiumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PdfiumError::Init(msg) => write!(f, "PDFium 初始化失败:{msg}"),
            PdfiumError::Load { path, reason } => {
                write!(f, "打开 PDF 失败 {}:{reason}", path.display())
            }
            PdfiumError::Render(msg) => write!(f, "PDF 渲染失败:{msg}"),
            PdfiumError::Text(msg) => write!(f, "PDF 文本提取失败:{msg}"),
            PdfiumError::Io(e) => write!(f, "PDFium 文件操作失败:{e}"),
        }
    }
}

impl std::error::Error for PdfiumError {}

/// 单个文本字符及其在 PDF 用户空间(点)中的包围盒。
/// 坐标原点在页面左下角,y 向上。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TextChar {
    pub ch: char,
    pub left: f64,
    pub bottom: f64,
    pub right: f64,
    pub top: f64,
}

// ── 函数指针类型 ──

type FnInitLibrary = unsafe extern "C" fn();
type FnLoadDocument = unsafe extern "C" fn(*const c_char, *const c_char) -> FPDF_DOCUMENT;
type FnGetPageCount = unsafe extern "C" fn(FPDF_DOCUMENT) -> c_int;
type FnGetPageWidth = unsafe extern "C" fn(FPDF_PAGE) -> f64;
type FnGetPageHeight = unsafe extern "C" fn(FPDF_PAGE) -> f64;
type FnLoadPage = unsafe extern "C" fn(FPDF_DOCUMENT, c_int) -> FPDF_PAGE;
type FnClosePage = unsafe extern "C" fn(FPDF_PAGE);
type FnBitmapCreateEx =
    unsafe extern "C" fn(c_int, c_int, c_int, *mut c_void, c_int) -> FPDF_BITMAP;
type FnRenderPageBitmap =
    unsafe extern "C" fn(FPDF_BITMAP, FPDF_PAGE, c_int, c_int, c_int, c_int, c_int, c_int);
type FnBitmapDestroy = unsafe extern "C" fn(FPDF_BITMAP);
type FnCloseDocument = unsafe extern "C" fn(FPDF_DOCUMENT);
type FnTextLoadPage = unsafe extern "C" fn(FPDF_PAGE) -> FPDF_TEXTPAGE;
type FnTextCountChars = unsafe extern "C" fn(FPDF_TEXTPAGE) -> c_int;
type FnTextGetUnicode = unsafe extern "C" fn(FPDF_TEXTPAGE, c_int) -> c_uint;
type FnTextGetCharBox =
    unsafe extern "C" fn(FPDF_TEXTPAGE, c_int, *mut f64, *mut f64, *mut f64, *mut f64) -> c_int;
type FnTextClosePage = unsafe extern "C" fn(FPDF_TEXTPAGE);

#[derive(Clone, Copy)]
struct Bindings {
    init_library: FnInitLibrary,
    load_document: FnLoadDocument,
    get_page_count: FnGetPageCount,
    get_page_width: FnGetPageWidth,
    get_page_height: FnGetPageHeight,
    load_page: FnLoadPage,
    close_page: FnClosePage,
    bitmap_create_ex: FnBitmapCreateEx,
    render_page_bitmap: FnRenderPageBitmap,
    bitmap_destroy: FnBitmapDestroy,
    close_document: FnCloseDocument,
    text_load_page: FnTextLoadPage,
    text_count_chars: FnTextCountChars,
    text_get_unicode: FnTextGetUnicode,
    text_get_char_box: FnTextGetCharBox,
    text_close_page: FnTextClosePage,
}

// ── Pdfium 实例 ──

pub struct Pdfium {
    /// 保持动态库句柄存活(字段顺序:library 先于 bindings,析构时 library 最后释放)
    _library: Library,
    bindings: Bindings,
}

impl Pdfium {
    /// 从嵌入字节(或 PDFIUM_LIB_PATH)加载 PDFium 并初始化。
    pub(crate) fn load() -> Result<Self, PdfiumError> {
        let path = resolve_library_path()?;
        Self::load_from_path_impl(&path)
    }

    /// 从显式路径加载(测试与 PDFIUM_LIB_PATH 使用)。
    #[cfg(test)]
    pub fn load_from_path(path: &Path) -> Result<Self, PdfiumError> {
        Self::load_from_path_impl(path)
    }

    fn load_from_path_impl(path: &Path) -> Result<Self, PdfiumError> {
        // libloading 的 Library::new 在 macOS 上等价于 dlopen,成功后库句柄保持存活。
        let library = unsafe { Library::new(path) }.map_err(|e| {
            PdfiumError::Init(format!("无法加载 PDFium 动态库 {}:{e}", path.display()))
        })?;

        let bindings = unsafe {
            Bindings {
                init_library: binding(&library, b"FPDF_InitLibrary\0").map_err(bind_err)?,
                load_document: binding(&library, b"FPDF_LoadDocument\0").map_err(bind_err)?,
                get_page_count: binding(&library, b"FPDF_GetPageCount\0").map_err(bind_err)?,
                get_page_width: binding(&library, b"FPDF_GetPageWidth\0").map_err(bind_err)?,
                get_page_height: binding(&library, b"FPDF_GetPageHeight\0").map_err(bind_err)?,
                load_page: binding(&library, b"FPDF_LoadPage\0").map_err(bind_err)?,
                close_page: binding(&library, b"FPDF_ClosePage\0").map_err(bind_err)?,
                bitmap_create_ex: binding(&library, b"FPDFBitmap_CreateEx\0").map_err(bind_err)?,
                render_page_bitmap: binding(&library, b"FPDF_RenderPageBitmap\0")
                    .map_err(bind_err)?,
                bitmap_destroy: binding(&library, b"FPDFBitmap_Destroy\0").map_err(bind_err)?,
                close_document: binding(&library, b"FPDF_CloseDocument\0").map_err(bind_err)?,
                text_load_page: binding(&library, b"FPDFText_LoadPage\0").map_err(bind_err)?,
                text_count_chars: binding(&library, b"FPDFText_CountChars\0").map_err(bind_err)?,
                text_get_unicode: binding(&library, b"FPDFText_GetUnicode\0").map_err(bind_err)?,
                text_get_char_box: binding(&library, b"FPDFText_GetCharBox\0").map_err(bind_err)?,
                text_close_page: binding(&library, b"FPDFText_ClosePage\0").map_err(bind_err)?,
            }
        };

        unsafe {
            (bindings.init_library)();
        }

        Ok(Self {
            _library: library,
            bindings,
        })
    }

    // ── 内部操作(调用方已持有全局 Mutex) ──

    fn open_document(&self, path: &Path) -> Result<PdfDocument, PdfiumError> {
        let cpath =
            CString::new(path.to_string_lossy().as_bytes()).map_err(|_| PdfiumError::Load {
                path: path.to_path_buf(),
                reason: "路径包含 NUL 字节".into(),
            })?;
        unsafe {
            let handle = (self.bindings.load_document)(cpath.as_ptr(), std::ptr::null());
            if handle.is_null() {
                return Err(PdfiumError::Load {
                    path: path.to_path_buf(),
                    reason: "FPDF_LoadDocument 返回空句柄(文件损坏、密码保护或路径不支持)".into(),
                });
            }
            let count = (self.bindings.get_page_count)(handle);
            if count <= 0 {
                (self.bindings.close_document)(handle);
                return Err(PdfiumError::Load {
                    path: path.to_path_buf(),
                    reason: "PDF 没有可读取的页面".into(),
                });
            }
            Ok(PdfDocument {
                handle,
                page_count: count as u32,
            })
        }
    }

    fn page_text(&self, doc: FPDF_DOCUMENT, page_index: u32) -> Result<String, PdfiumError> {
        unsafe {
            let page = (self.bindings.load_page)(doc, page_index as c_int);
            if page.is_null() {
                return Err(PdfiumError::Text(format!(
                    "FPDF_LoadPage({page_index}) 返回空句柄"
                )));
            }
            let text_page = (self.bindings.text_load_page)(page);
            if text_page.is_null() {
                (self.bindings.close_page)(page);
                return Err(PdfiumError::Text(format!(
                    "FPDFText_LoadPage({page_index}) 返回空句柄"
                )));
            }

            let count = (self.bindings.text_count_chars)(text_page);
            let mut text = String::with_capacity(count.max(0) as usize);
            for i in 0..count {
                let cp = (self.bindings.text_get_unicode)(text_page, i);
                if let Some(ch) = char::from_u32(cp) {
                    text.push(ch);
                }
            }

            (self.bindings.text_close_page)(text_page);
            (self.bindings.close_page)(page);
            Ok(text)
        }
    }

    fn page_text_chars(
        &self,
        doc: FPDF_DOCUMENT,
        page_index: u32,
    ) -> Result<Vec<TextChar>, PdfiumError> {
        unsafe {
            let page = (self.bindings.load_page)(doc, page_index as c_int);
            if page.is_null() {
                return Err(PdfiumError::Text(format!(
                    "FPDF_LoadPage({page_index}) 返回空句柄"
                )));
            }
            let text_page = (self.bindings.text_load_page)(page);
            if text_page.is_null() {
                (self.bindings.close_page)(page);
                return Err(PdfiumError::Text(format!(
                    "FPDFText_LoadPage({page_index}) 返回空句柄"
                )));
            }

            let count = (self.bindings.text_count_chars)(text_page);
            let mut chars = Vec::with_capacity(count.max(0) as usize);
            for i in 0..count {
                let cp = (self.bindings.text_get_unicode)(text_page, i);
                let Some(ch) = char::from_u32(cp) else {
                    continue;
                };
                if ch == '\0' {
                    continue;
                }
                let mut left = 0.0;
                let mut right = 0.0;
                let mut bottom = 0.0;
                let mut top = 0.0;
                let ok = (self.bindings.text_get_char_box)(
                    text_page,
                    i,
                    &mut left,
                    &mut right,
                    &mut bottom,
                    &mut top,
                );
                if ok == 0 {
                    continue;
                }
                chars.push(TextChar {
                    ch,
                    left,
                    bottom,
                    right,
                    top,
                });
            }

            (self.bindings.text_close_page)(text_page);
            (self.bindings.close_page)(page);
            Ok(chars)
        }
    }

    fn page_size(&self, doc: FPDF_DOCUMENT, page_index: u32) -> Result<(f32, f32), PdfiumError> {
        unsafe {
            let page = (self.bindings.load_page)(doc, page_index as c_int);
            if page.is_null() {
                return Err(PdfiumError::Render(format!(
                    "FPDF_LoadPage({page_index}) 返回空句柄"
                )));
            }
            let w = (self.bindings.get_page_width)(page) as f32;
            let h = (self.bindings.get_page_height)(page) as f32;
            (self.bindings.close_page)(page);
            Ok((w, h))
        }
    }

    fn render_page_png(
        &self,
        doc: FPDF_DOCUMENT,
        page_index: u32,
        scale: f32,
    ) -> Result<Vec<u8>, PdfiumError> {
        let scale = scale.max(0.25).min(8.0);
        let (width_pt, height_pt) = self.page_size(doc, page_index)?;
        let width = ((width_pt as f64 * scale as f64).round().max(1.0)) as c_int;
        let height = ((height_pt as f64 * scale as f64).round().max(1.0)) as c_int;

        unsafe {
            let page = (self.bindings.load_page)(doc, page_index as c_int);
            if page.is_null() {
                return Err(PdfiumError::Render(format!(
                    "FPDF_LoadPage({page_index}) 返回空句柄"
                )));
            }

            // PDFium 不会主动清空背景,先填白色,否则透明背景会变成黑底
            let mut buffer = vec![255u8; (width as usize) * (height as usize) * 4];
            let bitmap = (self.bindings.bitmap_create_ex)(
                width,
                height,
                FPDF_BITMAP_BGRX,
                buffer.as_mut_ptr() as *mut c_void,
                width * 4,
            );
            if bitmap.is_null() {
                (self.bindings.close_page)(page);
                return Err(PdfiumError::Render(format!(
                    "FPDFBitmap_CreateEx({width}x{height}) 返回空句柄"
                )));
            }

            (self.bindings.render_page_bitmap)(
                bitmap, page, 0, 0, width, height, 0, /* rotate */
                0, /* flags */
            );
            (self.bindings.bitmap_destroy)(bitmap);
            (self.bindings.close_page)(page);

            // BGRx → RGB(去掉未用的 X 通道,PNG 更小、编码更快)
            let mut rgb = Vec::with_capacity(buffer.len() / 4 * 3);
            for px in buffer.chunks_exact(4) {
                rgb.extend_from_slice(&[px[2], px[1], px[0]]);
            }
            encode_png(width as u32, height as u32, rgb)
        }
    }

    fn close_document(&self, doc: FPDF_DOCUMENT) {
        unsafe {
            (self.bindings.close_document)(doc);
        }
    }
}

unsafe fn binding<T: Copy>(lib: &Library, name: &[u8]) -> Result<T, libloading::Error> {
    let symbol: Symbol<T> = unsafe { lib.get(name)? };
    Ok(*symbol)
}

fn bind_err(e: libloading::Error) -> PdfiumError {
    PdfiumError::Init(format!("加载 PDFium 函数符号失败:{e}"))
}

// ── 全局单例与安全入口 ──

static PDFIUM: OnceLock<Mutex<Pdfium>> = OnceLock::new();

fn with_pdfium<T>(f: impl FnOnce(&Pdfium) -> Result<T, PdfiumError>) -> Result<T, PdfiumError> {
    let pdfium = match PDFIUM.get() {
        Some(p) => p,
        None => {
            let init = Mutex::new(Pdfium::load()?);
            let _ = PDFIUM.set(init);
            PDFIUM.get().expect("PDFIUM OnceLock 刚写入,必然可取到")
        }
    };
    let guard = pdfium
        .lock()
        .map_err(|_| PdfiumError::Init("pdfium mutex poisoned".into()))?;
    f(&guard)
}

/// 打开 PDF 文件,返回文档句柄(所有页面操作都会串行访问 PDFium)。
pub fn open(path: &Path) -> Result<PdfDocument, PdfiumError> {
    with_pdfium(|pdf| pdf.open_document(path))
}

/// 已打开的 PDF 文档。句柄通过全局 Mutex 串行访问,可跨线程 Send。
#[derive(Debug)]
pub struct PdfDocument {
    handle: FPDF_DOCUMENT,
    page_count: u32,
}

// 所有 PDFium 调用都经过全局 Mutex,原始句柄仅在文档存活期间使用,
// 因此可以安全地在线程间移动。
unsafe impl Send for PdfDocument {}
unsafe impl Sync for PdfDocument {}

impl PdfDocument {
    pub fn page_count(&self) -> u32 {
        self.page_count
    }

    /// 提取第 page_index 页(0-based)的文本。
    pub fn page_text(&self, page_index: u32) -> Result<String, PdfiumError> {
        if page_index >= self.page_count {
            return Err(PdfiumError::Text(format!(
                "page {page_index} out of range (count={})",
                self.page_count
            )));
        }
        with_pdfium(|pdf| pdf.page_text(self.handle, page_index))
    }

    /// 提取第 page_index 页(0-based)的字符及包围盒(PDF 用户空间坐标,原点左下)。
    pub fn page_text_chars(&self, page_index: u32) -> Result<Vec<TextChar>, PdfiumError> {
        if page_index >= self.page_count {
            return Err(PdfiumError::Text(format!(
                "page {page_index} out of range (count={})",
                self.page_count
            )));
        }
        with_pdfium(|pdf| pdf.page_text_chars(self.handle, page_index))
    }

    /// 以 scale(像素/点,1.0 = 72dpi)渲染第 page_index 页为 PNG 字节。
    pub fn render_page_png(&self, page_index: u32, scale: f32) -> Result<Vec<u8>, PdfiumError> {
        if page_index >= self.page_count {
            return Err(PdfiumError::Render(format!(
                "page {page_index} out of range (count={})",
                self.page_count
            )));
        }
        with_pdfium(|pdf| pdf.render_page_png(self.handle, page_index, scale))
    }

    /// 第 page_index 页(0-based)的尺寸(点)。
    pub fn page_size(&self, page_index: u32) -> Result<(f32, f32), PdfiumError> {
        if page_index >= self.page_count {
            return Err(PdfiumError::Render(format!(
                "page {page_index} out of range (count={})",
                self.page_count
            )));
        }
        with_pdfium(|pdf| pdf.page_size(self.handle, page_index))
    }
}

impl Drop for PdfDocument {
    fn drop(&mut self) {
        let handle = self.handle;
        let _ = with_pdfium(|pdf| {
            pdf.close_document(handle);
            Ok(())
        });
    }
}

// ── 库路径解析与解压 ──

fn resolve_library_path() -> Result<PathBuf, PdfiumError> {
    if let Ok(override_path) = env::var("PDFIUM_LIB_PATH") {
        let path = PathBuf::from(&override_path);
        if path.is_file() {
            return Ok(path);
        }
        return Err(PdfiumError::Init(format!(
            "PDFIUM_LIB_PATH 指向的文件不存在:{}",
            path.display()
        )));
    }

    let cache_dir = layout::home_dir()
        .join("Library/Caches/ueberneon")
        .join(format!("pdfium-{PDFIUM_VERSION}"));
    let dest = cache_dir.join("libpdfium.dylib");
    if is_plausible(&dest) {
        return Ok(dest);
    }

    fs::create_dir_all(&cache_dir).map_err(PdfiumError::Io)?;
    let tmp = cache_dir.join(format!(".libpdfium.{}.tmp", std::process::id()));
    fs::write(&tmp, EMBEDDED_PDFIUM).map_err(PdfiumError::Io)?;
    fs::rename(&tmp, &dest).map_err(PdfiumError::Io)?;

    if is_plausible(&dest) {
        Ok(dest)
    } else {
        Err(PdfiumError::Init("嵌入的 PDFium 动态库解压后异常".into()))
    }
}

fn is_plausible(path: &Path) -> bool {
    path.is_file()
        && path
            .metadata()
            .map(|m| m.len() > MIN_LIB_BYTES)
            .unwrap_or(false)
}

fn encode_png(width: u32, height: u32, rgb: Vec<u8>) -> Result<Vec<u8>, PdfiumError> {
    use image::{DynamicImage, ImageFormat, RgbImage};

    let img = RgbImage::from_raw(width, height, rgb)
        .ok_or_else(|| PdfiumError::Render("PNG 像素缓冲区尺寸不匹配".into()))?;
    let mut out = Vec::new();
    DynamicImage::ImageRgb8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), ImageFormat::Png)
        .map_err(|e| PdfiumError::Render(format!("PNG 编码失败:{e}")))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_pdf() -> PathBuf {
        let bytes = include_bytes!("../tests/fixtures/sample.pdf");
        let dir =
            std::env::temp_dir().join(format!("ueberneon-pdfium-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.pdf");
        fs::write(&path, bytes).unwrap();
        path
    }

    #[test]
    fn load_from_missing_path_returns_error() {
        match Pdfium::load_from_path(Path::new("/nonexistent/libpdfium.dylib")) {
            Err(PdfiumError::Init(_)) => {}
            _ => panic!("expected Init error for missing dylib"),
        }
    }

    #[test]
    fn open_extract_text_and_render_png() {
        let path = sample_pdf();
        let doc = open(&path).expect("打开 sample.pdf");
        assert_eq!(doc.page_count(), 1);

        let text = doc.page_text(0).expect("提取文本");
        assert!(text.contains("Hello PDFium"), "提取到的文本不完整:{text:?}");

        let png_1x = doc.render_page_png(0, 1.0).expect("渲染 1x");
        let png_2x = doc.render_page_png(0, 2.0).expect("渲染 2x");
        assert!(png_1x.starts_with(b"\x89PNG"), "不是 PNG 数据");
        assert!(png_2x.starts_with(b"\x89PNG"));

        let img_1x = image::load_from_memory(&png_1x).expect("解码 1x PNG");
        let img_2x = image::load_from_memory(&png_2x).expect("解码 2x PNG");
        assert!(img_2x.width() > img_1x.width());
        assert!(img_2x.height() > img_1x.height());

        // 背景应为白色,且页面有实际内容(不能是黑底)
        let rgb = img_1x.to_rgb8();
        let (w, h) = (rgb.width(), rgb.height());
        assert_eq!(rgb.get_pixel(0, 0).0, [255, 255, 255], "左上角应为白色背景");
        assert_eq!(
            rgb.get_pixel(w - 1, h - 1).0,
            [255, 255, 255],
            "右下角应为白色背景"
        );
        let non_white = rgb.pixels().filter(|p| p.0 != [255, 255, 255]).count();
        assert!(
            non_white > 0 && non_white < rgb.pixels().count() / 2,
            "页面渲染异常:非白像素 {non_white}/{}",
            rgb.pixels().count()
        );
    }

    #[test]
    fn page_text_chars_returns_ordered_boxes_within_page() {
        let path = sample_pdf();
        let doc = open(&path).unwrap();
        let chars = doc.page_text_chars(0).expect("page_text_chars");

        let text: String = chars.iter().map(|c| c.ch).collect();
        assert!(text.contains("Hello PDFium"), "字符序列不完整:{text:?}");

        let (w, h) = doc.page_size(0).unwrap();
        for c in &chars {
            assert!(
                c.left >= 0.0 && c.right <= w as f64,
                "left/right 越界:{c:?}"
            );
            assert!(
                c.bottom >= 0.0 && c.top <= h as f64,
                "bottom/top 越界:{c:?}"
            );
            assert!(c.right > c.left && c.top > c.bottom, "字符盒异常:{c:?}");
        }
    }
}
