// ── 共享 ONNX Runtime 引导 ──
//
// ort 的 `init_from(...).commit()` 是进程级全局操作,公式 OCR 与页面 OCR
// 共用同一个 runtime:第一次加载的 libonnxruntime.dylib 生效,后续请求
// 不同库路径时只记录警告并沿用已加载的库,避免重复 commit 导致崩溃。

use std::path::Path;
use std::sync::{Mutex, OnceLock};

static RUNTIME: OnceLock<Mutex<Option<String>>> = OnceLock::new();

/// 确保 ONNX Runtime 已初始化;`lib_path` 为模型包内的动态库。
/// 首次调用完成全局初始化,后续调用幂等。
pub fn ensure_initialized(lib_path: &Path) -> Result<(), String> {
    let requested = lib_path.to_string_lossy().into_owned();
    let mut guard = RUNTIME
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map_err(|_| "onnx runtime lock poisoned".to_string())?;
    if let Some(active) = guard.as_ref() {
        if active != &requested {
            tracing::warn!(
                "ONNX Runtime 已由 {} 初始化,忽略新库路径 {}",
                active,
                requested
            );
        }
        return Ok(());
    }
    let committed = ort::init_from(lib_path)
        .map_err(|e| format!("init_from({lib_path:?}): {e}"))?
        .commit();
    if !committed {
        return Err(format!("commit ONNX Runtime 失败({lib_path:?})"));
    }
    *guard = Some(requested);
    Ok(())
}
