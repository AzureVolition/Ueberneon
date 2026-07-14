// ls 工具 —— 列出目录内容。
//
// 支持递归模式和单层列表，自动跳过噪声目录。
// 目录名后加 `/`，文件名后跟制表符和字节大小。

use std::path::Path;

use llm::tool::{AgentMode, Tool, ToolContext, ToolResult};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;

/// ls — 列出目录内容。
///
/// 目录名后加 `/`，文件名后跟制表符和字节大小。
/// 递归模式跳过 .git、node_modules 等噪声目录。
#[derive(ToolMetaImpl)]
pub struct Ls {
    schema: Value,
    read_only: bool,
}

/// 递归遍历时要跳过的噪声目录名。
const NOISE_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "__pycache__",
    ".idea",
    ".vscode",
    ".DS_Store",
    "target",
    "dist",
    "build",
    ".next",
    "vendor",
    "coverage",
];

impl Ls {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Directory path (default \".\")"
                    },
                    "recursive": {
                        "type": "boolean",
                        "description": "When true, recursively list all nested files (default false)"
                    }
                }
            }),
            read_only: true,
        }
    }
}

#[async_trait::async_trait]
impl Tool for Ls {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> ToolResult {
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            .unwrap_or(".");

        let recursive = args
            .get("recursive")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let path = std::path::Path::new(path_str);

        // 安全检查
        if path.components().any(|c| c.as_os_str() == ".git") {
            return ToolResult::blocked("access to .git directory is not allowed");
        }

        if !path.exists() {
            return ToolResult::err(format!("ls: path '{}' does not exist", path_str));
        }

        if !path.is_dir() {
            return ToolResult::err(format!("ls: '{}' is not a directory", path_str));
        }

        if recursive {
            list_recursive(path, path_str)
        } else {
            list_flat(path, path_str)
        }
    }
}

/// 非递归：读取单层目录。
fn list_flat(dir: &Path, display: &str) -> ToolResult {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => return ToolResult::err(format!("ls: failed to read '{}': {}", display, e)),
    };

    let mut items: Vec<String> = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let name = entry.file_name().to_string_lossy().to_string();
        let metadata = match entry.metadata() {
            Ok(m) => m,
            Err(_) => {
                items.push(name);
                continue;
            }
        };

        if metadata.is_dir() {
            items.push(format!("{}/", name));
        } else {
            items.push(format!("{}\t{}", name, metadata.len()));
        }
    }

    items.sort();

    if items.is_empty() {
        return ToolResult::ok("(empty directory)");
    }

    ToolResult::ok(items.join("\n"))
}

/// 递归：深度优先遍历目录树。
fn list_recursive(dir: &Path, display: &str) -> ToolResult {
    let mut walker = walkdir::WalkDir::new(dir)
        .sort_by(|a, b| a.file_name().cmp(b.file_name()))
        .into_iter();

    let mut items: Vec<String> = Vec::new();

    loop {
        let entry = match walker.next() {
            Some(Ok(e)) => e,
            Some(Err(_)) => continue,
            None => break,
        };

        let depth = entry.depth();
        if depth == 0 {
            continue; // 跳过根目录自身
        }

        let file_name = entry.file_name().to_string_lossy();
        if entry.file_type().is_dir() && NOISE_DIRS.contains(&file_name.as_ref()) {
            // 标记噪声目录并跳过其子树
            let indent = "    ".repeat(depth.saturating_sub(1));
            items.push(format!("{}{}/", indent, file_name));
            walker.skip_current_dir();
            continue;
        }

        let indent = "    ".repeat(depth.saturating_sub(1));

        if entry.file_type().is_dir() {
            items.push(format!("{}{}/", indent, file_name));
        } else {
            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(_) => {
                    items.push(format!("{}{}", indent, file_name));
                    continue;
                }
            };
            items.push(format!("{}{}\t{}", indent, file_name, meta.len()));
        }
    }

    if items.is_empty() {
        return ToolResult::ok("(empty directory tree)");
    }

    ToolResult::ok(items.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};
    use llm::tool::ToolMeta;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("_test_ls_{}_{}", std::process::id(), id))
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
    async fn list_flat_dir() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello").unwrap();
        std::fs::write(dir.join("b.rs"), b"fn main() {}").unwrap();
        std::fs::create_dir(dir.join("subdir")).unwrap();

        let tool = Ls::new();
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"path": dir.to_str().unwrap()}),
        ).await;

        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("a.txt"), "output: {}", result.output());
        assert!(result.output().contains("b.rs"), "output: {}", result.output());
        assert!(result.output().contains("subdir/"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_empty_dir() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();

        let tool = Ls::new();
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"path": dir.to_str().unwrap()}),
        ).await;

        assert!(result.error().is_none());
        assert!(result.output().contains("empty"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn list_recursive() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("root.txt"), b"root").unwrap();
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("sub").join("nested.rs"), b"nested").unwrap();

        let tool = Ls::new();
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({
                "path": dir.to_str().unwrap(),
                "recursive": true,
            }),
        ).await;

        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("root.txt"), "output: {}", result.output());
        assert!(result.output().contains("sub/"), "output: {}", result.output());
        assert!(result.output().contains("nested.rs"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn path_not_exists() {
        let tool = Ls::new();
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"path": "/nonexistent_path_12345"}),
        ).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("does not exist"));
    }

    #[tokio::test]
    async fn path_is_file() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("test.txt");
        std::fs::write(&file, b"content").unwrap();

        let tool = Ls::new();
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"path": file.to_str().unwrap()}),
        ).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("not a directory"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reject_git_path() {
        let tool = Ls::new();
        let result = tool.execute(
            &test_ctx(),
            &serde_json::json!({"path": "/tmp/repo/.git"}),
        ).await;
        assert!(result.is_blocked());
    }

    #[test]
    fn schema_is_valid_json() {
        let tool = Ls::new();
        let schema = tool.schema();
        assert!(schema.is_object());
        assert_eq!(schema["type"], "object");
        assert!(tool.read_only());
    }

    #[tokio::test]
    async fn default_path_is_dot() {
        let tool = Ls::new();
        let result = tool.execute(
            &ToolContext { call_id: "test".into(), plan_mode: false, agent_mode: AgentMode::Ask, progress: None },
            &serde_json::json!({}),
        ).await;
        // 应该成功列出当前目录
        assert!(result.error().is_none(), "error: {:?}", result.error());
    }
}
