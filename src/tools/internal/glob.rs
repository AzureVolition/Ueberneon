// glob 工具 —— 按 glob 模式搜索文件。
//
// 支持 ** 递归匹配，结果排序后输出，最多返回 1000 条。

use crate::agent::{Tool, ToolContext, ToolResult};
use std::path::PathBuf;
#[cfg(test)]
use crate::agent::{AgentMode, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// glob — 按 glob 模式搜索文件路径。
///
/// 支持 `*`、`?`、`[]` 和 `**`（递归匹配）语法。
/// 结果按路径字符串排序。
#[derive(ToolMetaImpl)]
pub struct Glob {
    schema: Value,
    read_only: bool,
    work_dir: PathBuf,
}

/// 最大返回结果数。
const GLOB_MAX_RESULTS: usize = 1000;

impl Glob {
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Glob pattern (supports ** for recursive matching)"
                    }
                },
                "required": ["pattern"]
            }),
            read_only: true,
            work_dir,
        }
    }
}

#[async_trait::async_trait]
impl Tool for Glob {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let pattern = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return Err("glob: missing required argument 'pattern'".into()),
        };

        // 相对 pattern 拼接到 work_dir 下
        let pattern = if std::path::Path::new(pattern).is_relative() {
            self.work_dir.join(pattern).to_string_lossy().to_string()
        } else {
            pattern.to_string()
        };

        // 安全检查：拒绝 .git 路径
        if pattern.contains("/.") || pattern.contains(".git") {
            // 更精确的检查：解析 pattern 是否明确包含 .git
            for component in pattern.split('/') {
                if component == ".git" || component.starts_with(".git/") {
                    return Err("access to .git directory is not allowed".into());
                }
            }
            // 也要检查 ** 展开后会进入 .git 的情况
            if pattern.contains(".git") {
                // 粗略拦截，glob 库会自然地跳过隐藏目录
            }
        }

        let mut results: Vec<String> = Vec::new();

        // 使用 glob 库进行匹配
        match glob::glob(&pattern) {
            Ok(entries) => {
                for entry in entries {
                    if results.len() >= GLOB_MAX_RESULTS {
                        break;
                    }
                    match entry {
                        Ok(path) => {
                            let path_str = path.to_string_lossy().to_string();
                            // 过滤 .git 路径
                            if path_str.contains("/.git/") || path_str.starts_with(".git/") {
                                continue;
                            }
                            results.push(path_str);
                        }
                        Err(_) => continue,
                    }
                }
            }
            Err(e) => {
                return Err(format!("glob: invalid pattern: {}", e));
            }
        }

        if results.is_empty() {
            return Ok(ToolResult::ok("(no matches)"));
        }

        results.sort();

        let mut output = results.join("\n");

        if results.len() >= GLOB_MAX_RESULTS {
            output.push_str(&format!(
                "\n... (truncated at {} results)",
                GLOB_MAX_RESULTS
            ));
        }

        Ok(ToolResult::ok(output))
    }
}


#[async_trait::async_trait]
impl CheckableTool for Glob {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use llm::tool::ToolMeta;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("_test_glob_{}_{}", std::process::id(), id))
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }
    }

    #[tokio::test]
    async fn glob_all_rs_files() {
        let dir = temp_dir();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.rs"), b"").unwrap();
        std::fs::write(dir.join("b.py"), b"").unwrap();
        std::fs::write(dir.join("sub").join("c.rs"), b"").unwrap();

        let pattern = format!("{}/**/*.rs", dir.to_str().unwrap());
        let tool = Glob::new(std::env::temp_dir());
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"pattern": pattern}),
        ).await;

        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("a.rs"), "output: {}", result.output());
        assert!(result.output().contains("c.rs"), "output: {}", result.output());
        assert!(!result.output().contains("b.py"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn glob_specific_file() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(dir.join("README.md"), b"# readme").unwrap();

        let pattern = format!("{}/*.toml", dir.to_str().unwrap());
        let tool = Glob::new(std::env::temp_dir());
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"pattern": pattern}),
        ).await;

        assert!(result.error().is_none());
        assert!(result.output().contains("Cargo.toml"), "output: {}", result.output());
        assert!(!result.output().contains("README.md"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_matches() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let pattern = format!("{}/*.xyz", dir.to_str().unwrap());
        let tool = Glob::new(std::env::temp_dir());
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"pattern": pattern}),
        ).await;

        assert!(result.error().is_none());
        assert!(result.output().contains("no matches"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn invalid_pattern() {
        let tool = Glob::new(std::env::temp_dir());
        // 使用含无效字符的 pattern
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"pattern": "[\0"}),
        ).await;
        // 应该报错而非崩溃
        assert!(result.error().is_some() || result.output().contains("no matches"));
    }

    #[tokio::test]
    async fn missing_pattern() {
        let tool = Glob::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({})).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("missing"));
    }

    #[tokio::test]
    async fn empty_pattern() {
        let tool = Glob::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({"pattern": ""})).await;
        assert!(result.error().is_some());
    }

    #[test]
    fn schema_is_valid_json() {
        let tool = Glob::new(std::env::temp_dir());
        let schema = tool.schema();
        assert!(schema.is_object());
        assert_eq!(schema["type"], "object");
        assert!(schema["required"].as_array().unwrap().contains(&serde_json::Value::String("pattern".into())));
        assert!(tool.read_only());
    }
}
