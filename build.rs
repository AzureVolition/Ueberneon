// ── PDFium 构建集成 ──
//
// 将 PDFium 动态库嵌入最终二进制:
// - 优先使用 PDFIUM_BUNDLE_LIB 指向的本地 dylib(跳过网络);
// - 否则从 bblanchon/pdfium-binaries 下载 chromium/7961 的
//   pdfium-mac-arm64.tgz,校验 SHA-256 后解压缓存到
//   $CARGO_HOME/ueberneon-pdfium/7961/,并复制到 OUT_DIR 供 include_bytes 嵌入。
//
// v1 仅支持 macOS arm64;其它 target 构建时直接报错。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

const PDFIUM_VERSION: &str = "7961";
const ARCHIVE_SHA256: &str = "1193a771e0bd934530afa3df73a0d44551d8f4078442e290054e6dd38ded960f";
const ARCHIVE_URL: &str = "https://github.com/bblanchon/pdfium-binaries/releases/download/chromium%2F7961/pdfium-mac-arm64.tgz";
const MIN_BYTES: u64 = 1_000_000;

fn main() {
    println!("cargo:rerun-if-env-changed=PDFIUM_BUNDLE_LIB");
    println!("cargo:rerun-if-env-changed=PDFIUM_BUILD_CACHE_DIR");
    println!("cargo:rerun-if-env-changed=UEBERNEON_FORMULA_BUNDLE_DIR");
    println!("cargo:rerun-if-env-changed=UEBERNEON_FORMULA_CACHE_DIR");

    let target = env::var("TARGET").unwrap_or_default();
    if target != "aarch64-apple-darwin" {
        panic!(
            "ueberneon 的 PDFium 支持目前仅限 macOS arm64 \
             (TARGET=aarch64-apple-darwin),当前 TARGET={target}"
        );
    }

    let source = if let Ok(bundle) = env::var("PDFIUM_BUNDLE_LIB") {
        let path = PathBuf::from(&bundle);
        assert!(
            path.is_file(),
            "PDFIUM_BUNDLE_LIB 指向的文件不存在: {}",
            path.display()
        );
        println!("cargo:rerun-if-changed={}", path.display());
        path
    } else {
        let cache_root = env::var_os("PDFIUM_BUILD_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                let cargo_home =
                    env::var_os("CARGO_HOME")
                        .map(PathBuf::from)
                        .unwrap_or_else(|| {
                            let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                            PathBuf::from(home).join(".cargo")
                        });
                cargo_home.join("ueberneon-pdfium").join(PDFIUM_VERSION)
            });
        ensure_pdfium_dylib(&cache_root)
    };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR 未设置"));
    fs::create_dir_all(&out_dir).expect("创建 OUT_DIR 失败");
    fs::copy(&source, out_dir.join("libpdfium.dylib"))
        .expect("复制 libpdfium.dylib 到 OUT_DIR 失败");

    write_formula_bundle(&out_dir);
}

/// 公式 OCR 资源(可选):libonnxruntime.dylib + model.onnx + dict.json + preprocess.json。
/// 未配置时生成空资源,运行时代码会优雅回退到文本层重建。
fn write_formula_bundle(out_dir: &Path) {
    let bundle = resolve_formula_bundle();
    let read = |name: &str, p: Option<&Path>| -> Vec<u8> {
        match p {
            Some(p) => fs::read(p)
                .unwrap_or_else(|e| panic!("读取公式资源 {name} 失败 {}: {e}", p.display())),
            None => Vec::new(),
        }
    };
    let code = format!(
        "pub static ONNXRUNTIME_DYLIB: &[u8] = &{:#?};\n\
         pub static FORMULA_MODEL: &[u8] = &{:#?};\n\
         pub static FORMULA_DICT: &[u8] = &{:#?};\n\
         pub static FORMULA_PREPROCESS: &[u8] = &{:#?};\n",
        read("libonnxruntime.dylib", bundle.lib.as_deref()),
        read("model.onnx", bundle.model.as_deref()),
        read("dict.json", bundle.dict.as_deref()),
        read("preprocess.json", bundle.preprocess.as_deref()),
    );
    fs::write(out_dir.join("bundled_formula.rs"), code).expect("写入 bundled_formula.rs 失败");
}

struct FormulaBundle {
    lib: Option<PathBuf>,
    model: Option<PathBuf>,
    dict: Option<PathBuf>,
    preprocess: Option<PathBuf>,
}

fn resolve_formula_bundle() -> FormulaBundle {
    const NAMES: [&str; 4] = [
        "libonnxruntime.dylib",
        "model.onnx",
        "dict.json",
        "preprocess.json",
    ];

    if let Ok(dir) = env::var("UEBERNEON_FORMULA_BUNDLE_DIR") {
        let dir = PathBuf::from(&dir);
        for name in NAMES {
            assert!(
                dir.join(name).is_file(),
                "UEBERNEON_FORMULA_BUNDLE_DIR 缺少 {name}: {}",
                dir.join(name).display()
            );
        }
        return FormulaBundle {
            lib: Some(dir.join(NAMES[0])),
            model: Some(dir.join(NAMES[1])),
            dict: Some(dir.join(NAMES[2])),
            preprocess: Some(dir.join(NAMES[3])),
        };
    }

    let cache_root = env::var_os("UEBERNEON_FORMULA_CACHE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let cargo_home = env::var_os("CARGO_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".into());
                    PathBuf::from(home).join(".cargo")
                });
            cargo_home
                .join("ueberneon-formula")
                .join("pp-formulanet-plus-s")
        });
    let paths: Vec<PathBuf> = NAMES.iter().map(|n| cache_root.join(n)).collect();
    if paths.iter().all(|p| p.is_file()) {
        return FormulaBundle {
            lib: Some(paths[0].clone()),
            model: Some(paths[1].clone()),
            dict: Some(paths[2].clone()),
            preprocess: Some(paths[3].clone()),
        };
    }
    FormulaBundle {
        lib: None,
        model: None,
        dict: None,
        preprocess: None,
    }
}

/// 确保缓存里有可用的 dylib,返回其路径。
fn ensure_pdfium_dylib(cache_root: &Path) -> PathBuf {
    let dylib = cache_root.join("libpdfium.dylib");
    if is_plausible(&dylib) {
        return dylib;
    }

    fs::create_dir_all(cache_root)
        .unwrap_or_else(|e| panic!("创建 PDFium 缓存目录失败 {}: {e}", cache_root.display()));

    let archive = cache_root.join(format!("pdfium-mac-arm64-{PDFIUM_VERSION}.tgz"));
    if !is_plausible(&archive) {
        download_archive(&archive);
    }
    verify_sha256(&archive);
    extract_dylib(&archive, cache_root);

    assert!(
        is_plausible(&dylib),
        "PDFium 解压后未找到 libpdfium.dylib: {}",
        dylib.display()
    );
    dylib
}

fn is_plausible(path: &Path) -> bool {
    path.is_file()
        && path
            .metadata()
            .map(|m| m.len() > MIN_BYTES)
            .unwrap_or(false)
}

fn download_archive(dest: &Path) {
    let status = Command::new("curl")
        .args(["-L", "--fail", "--silent", "--show-error", "-o"])
        .arg(dest)
        .arg(ARCHIVE_URL)
        .status()
        .expect("无法启动 curl 下载 PDFium;请先安装 curl");
    assert!(
        status.success(),
        "PDFium 下载失败:{ARCHIVE_URL}\n如需离线构建,请用 PDFIUM_BUNDLE_LIB 指定本地 dylib"
    );
}

fn verify_sha256(path: &Path) {
    let bytes =
        fs::read(path).unwrap_or_else(|e| panic!("读取 PDFium 归档失败 {}: {e}", path.display()));
    let digest = Sha256::digest(&bytes);
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        hex,
        ARCHIVE_SHA256,
        "PDFium 归档 SHA-256 校验失败,文件可能被篡改或下载不完整: {}",
        path.display()
    );
}

fn extract_dylib(archive: &Path, cache_root: &Path) {
    let status = Command::new("tar")
        .args(["-xzf"])
        .arg(archive)
        .args(["-C"])
        .arg(cache_root)
        .status()
        .expect("无法启动 tar 解压 PDFium 归档");
    assert!(
        status.success(),
        "PDFium 归档解压失败: {}",
        archive.display()
    );

    let extracted = cache_root.join("lib/libpdfium.dylib");
    assert!(
        extracted.is_file(),
        "归档中未找到 lib/libpdfium.dylib: {}",
        extracted.display()
    );
    let dest = cache_root.join("libpdfium.dylib");
    if dest.exists() {
        fs::remove_file(&dest).expect("清理旧 dylib 失败");
    }
    fs::rename(&extracted, &dest).expect("移动 libpdfium.dylib 到缓存根目录失败");
}
