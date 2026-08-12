// ── 应用设置 —— JSON 文件持久化 ──
//
// 存储路径：~/.ueberneon/settings.json
// 通用设置（General）+ 外观设置（Appearance）

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::OnceLock;

// ── 数据结构 ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppSettings {
    pub general: GeneralSettings,
    pub appearance: AppearanceSettings,
    /// 公式识别(ONNX)配置:模型目录,支持运行时切换其它模型。
    #[serde(default)]
    pub formula_ocr: FormulaOcrSettings,
    /// 页面 OCR(ONNX)配置:模型目录、自动整本 OCR、并行 worker 数。
    #[serde(default)]
    pub page_ocr: PageOcrSettings,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralSettings {
    /// 默认 Agent 配置 ID（对应 agent_configs 表）
    pub default_agent_config_id: String,
    /// 默认 Action Mode："regular" | "plan"
    pub default_action_mode: String,
    /// 默认 Agent Mode："cautious" | "ask" | "auto" | "unleashed"
    pub default_agent_mode: String,
    /// 默认 SubAgent Provider Instance ID（当 subagent 未配置时使用）
    pub default_subagent_provider_instance_id: String,
    /// 默认 SubAgent 模型（当 subagent 未配置时使用）
    pub default_subagent_model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppearanceSettings {
    /// 界面字体大小："xs" | "sm" | "md" | "lg" | "xl"
    pub font_size: String,
    /// 代码字体 key
    pub code_font: String,
    /// 界面密度："comfortable" | "compact"
    pub ui_density: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct FormulaOcrSettings {
    /// 模型目录,包含 manifest.json / model.onnx / tokenizer.json / libonnxruntime.dylib。
    /// 为空时自动扫描 ~/.ueberneon/formula-models/ 与 UEBERNEON_FORMULA_DIR。
    pub model_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PageOcrSettings {
    /// 模型目录,包含 manifest.json / det_model.onnx / rec_model.onnx / rec_dict.txt。
    /// 为空时自动扫描 ~/.ueberneon/page-ocr-models/ 与 UEBERNEON_PAGE_OCR_DIR。
    pub model_dir: Option<String>,
    /// 导入书后是否自动后台整本 OCR(无文本页)。
    pub auto_ocr: bool,
    /// 并行 worker 数(1..=4)。
    pub workers: u32,
}

impl Default for PageOcrSettings {
    fn default() -> Self {
        Self {
            model_dir: None,
            auto_ocr: true,
            workers: 3,
        }
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            general: GeneralSettings {
                default_agent_config_id: String::new(),
                default_action_mode: "regular".into(),
                default_agent_mode: "ask".into(),
                default_subagent_provider_instance_id: String::new(),
                default_subagent_model: String::new(),
            },
            appearance: AppearanceSettings {
                font_size: "md".into(),
                code_font: "jetbrains-mono".into(),
                ui_density: "comfortable".into(),
            },
            formula_ocr: FormulaOcrSettings::default(),
            page_ocr: PageOcrSettings::default(),
        }
    }
}

// ── 路径 ──

fn settings_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".ueberneon").join("settings.json")
}

// ── 全局单例 ──

static SETTINGS: OnceLock<Mutex<AppSettings>> = OnceLock::new();

fn global() -> &'static Mutex<AppSettings> {
    SETTINGS.get_or_init(|| {
        let path = settings_path();
        if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    if let Ok(s) = serde_json::from_str(&content) {
                        return Mutex::new(s);
                    }
                }
                Err(e) => {
                    eprintln!("settings: failed to read {}: {}", path.display(), e);
                }
            }
        }
        let default = AppSettings::default();
        let _ = save_inner(&default);
        Mutex::new(default)
    })
}

fn save_inner(s: &AppSettings) -> Result<(), String> {
    let path = settings_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| e.to_string())?;
    std::fs::write(&path, &json).map_err(|e| e.to_string())?;
    tracing::debug!(
        "[settings] saved: {}",
        json.lines().collect::<Vec<_>>().join(" ")
    );
    Ok(())
}

// ── 公开 API ──

/// 获取当前设置的克隆
pub fn get() -> AppSettings {
    global().lock().expect("settings lock poisoned").clone()
}

/// 更新设置（传入闭包修改），自动持久化
pub fn update(f: impl FnOnce(&mut AppSettings)) {
    let mut guard = global().lock().expect("settings lock poisoned");
    f(&mut guard);
    let cloned = guard.clone();
    drop(guard);
    if let Err(e) = save_inner(&cloned) {
        eprintln!("settings: failed to save: {e}");
    }
}

/// 获取当前设置的引用（临时读锁）
pub fn with<T>(f: impl FnOnce(&AppSettings) -> T) -> T {
    let guard = global().lock().expect("settings lock poisoned");
    f(&guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formula_ocr_defaults_to_none() {
        let s = AppSettings::default();
        assert!(s.formula_ocr.model_dir.is_none());
        assert!(s.page_ocr.auto_ocr);
        assert_eq!(s.page_ocr.workers, 3);
        assert!(s.page_ocr.model_dir.is_none());
    }

    #[test]
    fn old_settings_json_without_formula_ocr_still_loads() {
        let old = r#"{"general":{"default_agent_config_id":"","default_action_mode":"regular","default_agent_mode":"ask","default_subagent_provider_instance_id":"","default_subagent_model":""},"appearance":{"font_size":"md","code_font":"jetbrains-mono","ui_density":"comfortable"}}"#;
        let parsed: AppSettings = serde_json::from_str(old).unwrap();
        assert!(parsed.formula_ocr.model_dir.is_none());
        assert!(parsed.page_ocr.auto_ocr);
        assert_eq!(parsed.page_ocr.workers, 3);
    }
}
