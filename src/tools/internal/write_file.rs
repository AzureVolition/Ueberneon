// write_file 工具 —— 创建或覆盖文件。
//
// - 默认拒绝覆盖已有文件（须显式设置 overwrite: true）
// - 自动创建父目录
// - 写前通过 checkpoint 记录快照

use std::path::PathBuf;
use std::sync::Arc;

use llm::tool::{AgentMode, Tool, ToolContext, ToolMeta, ToolResult, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;

use crate::tools::content_tracker::FileObserveTracker;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::tools::snapshot::SnapshotStore;
use crate::tools::diff::{self, Kind as DiffKind};
use crate::permission::{Check, Decision, gate::PermissionChecked};

/// write_file — 创建新文件或覆盖已有文件。
///
/// 默认拒绝覆盖已有文件（使用 edit_file 进行定向编辑）；
/// 设置 `overwrite: true` 可以覆盖。
///
/// `work_dir` 是工作目录的共享引用 —— 所有文件路径必须在此目录之下。
#[derive(ToolMetaImpl)]
pub struct WriteFile {
    schema: Value,
    read_only: bool,
    /// 工作目录（共享引用语义）。
    work_dir: PathBuf,
    /// 检查点存储（写前记录快照）。
    checkpoint: Arc<SnapshotStore>,
    /// 权限检查列表。
    checks: Vec<Box<dyn Check>>,
    /// 文件内容追踪器（陈旧锚点检查）。
    tracker: Arc<FileObserveTracker>,
}

impl WriteFile {
    pub fn new(work_dir: PathBuf, checkpoint: Arc<SnapshotStore>, checks: Vec<Box<dyn Check>>, tracker: Arc<FileObserveTracker>) -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path"
                    },
                    "content": {
                        "type": "string",
                        "description": "File content to write"
                    },
                    "overwrite": {
                        "type": "boolean",
                        "description": "If true, overwrite an existing file (default: false)"
                    }
                },
                "required": ["path", "content"]
            }),
            read_only: false,
            work_dir,
            checkpoint,
            checks,
            tracker,
        }
    }

    /// 将路径解析为绝对路径：
    /// - 相对路径拼接到 work_dir 下
    /// - 绝对路径必须在 work_dir 内
    fn resolve_path(&self, path: &str) -> Result<PathBuf, String> {
        let p = std::path::Path::new(path);
        let abs = if p.is_relative() {
            let joined = self.work_dir.join(p);
            match std::fs::canonicalize(&joined) {
                Ok(c) => {
                    if !c.starts_with(&self.work_dir) {
                        return Err(format!(
                            "path '{}' is outside the workspace directory '{}'",
                            c.display(),
                            self.work_dir.display()
                        ));
                    }
                    c
                }
                Err(_) => {
                    let normalized = normalize_path(&joined);
                    if !normalized.starts_with(&self.work_dir) {
                        return Err(format!(
                            "path '{}' is outside the workspace directory '{}'",
                            normalized.display(),
                            self.work_dir.display()
                        ));
                    }
                    normalized
                }
            }
        } else {
            if !p.starts_with(&self.work_dir) {
                return Err(format!(
                    "path '{}' is outside the workspace directory '{}'",
                    p.display(),
                    self.work_dir.display()
                ));
            }
            p.to_path_buf()
        };

        Ok(abs)
    }
}

/// 规范化路径中的 `..` 和 `.` 组件（不要求文件存在）。
fn normalize_path(path: &std::path::Path) -> PathBuf {
    use std::path::Component;
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                if !components.is_empty() {
                    components.pop();
                }
            }
            Component::CurDir => {}
            other => components.push(other),
        }
    }
    let mut result = PathBuf::new();
    for component in components {
        result.push(component);
    }
    result
}

impl PermissionChecked for WriteFile {
    fn permission_checks(&self) -> &[Box<dyn Check>] {
        &self.checks
    }
}

#[async_trait::async_trait]
impl Tool for WriteFile {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        // 1. 解析参数
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return Err("write_file: missing required argument 'path'".into()),
        };
        let content = match args.get("content").and_then(|v| v.as_str()) {
            Some(c) => c,
            None => return Err("write_file: missing required argument 'content'".into()),
        };
        let overwrite = args.get("overwrite").and_then(|v| v.as_bool()).unwrap_or(false);

        // 2. 解析路径并检查范围
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return Err(format!("write_file: {}", e)),
        };

        // 3. 检查文件是否已存在
        if path.exists() && !overwrite {
            return Err(format!(
                "write_file: '{}' already exists. Use overwrite=true to replace, or use edit_file for targeted edits.",
                path_str
            ));
        }

        // 4. 记录 checkpoint 快照 + 陈旧锚点检查（如果文件已存在）
        let original_content = if path.exists() {
            match std::fs::read_to_string(&path) {
                Ok(c) => {
                    // 陈旧锚点检查：覆盖已有文件时验证内容是否一致
                    if let Err(msg) = self.tracker.check_anchor(path_str, &c) {
                        return Err(msg);
                    }
                    self.checkpoint.snapshot(path_str, &c, 0);
                    c
                }
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };

        // 5. 创建父目录
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    return Err(format!(
                        "write_file: failed to create directory '{}': {}",
                        parent.display(),
                        e
                    ));
                }
            }
        }

        // 6. 构建 diff（用于返回消息）
        let file_change = diff::build_diff(path_str, &original_content, content, DiffKind::Modify);

        // 7. 写入文件
        if let Err(e) = std::fs::write(&path, content) {
            return Err(format!("write_file: failed to write '{}': {}", path_str, e));
        }

        // 8. 更新追踪器
        self.tracker.record_write(path_str, content);

        // 9. 返回成功消息
        let summary = diff::change_summary(&file_change);
        Ok(ToolResult::ok(format!("wrote {}\n{}", path_str, summary)))
    }
}

#[async_trait::async_trait]
impl CheckableTool for WriteFile {
    fn check(&self, ctx: &ToolContext, args: &Value) -> Decision {
        match self.check_permission(self.name(), args, ctx.agent_mode) {
            Decision::Allow => {}
            decision => return decision,
        }
        if ctx.plan_mode {
            return Decision::Deny("write_file is not allowed in plan mode".into());
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use llm::tool::ToolMeta;

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir();
        let id = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        dir.join(format!("_test_write_file_{}_{}", std::process::id(), id))
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
    async fn create_new_file() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = WriteFile::new(work_dir.clone(), checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let path = work_dir.join("new_file.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "hello\nworld\n"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("wrote"), "output: {}", result.output());

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello\nworld\n");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn overwrite_existing_file() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let path = work_dir.join("existing.txt");
        std::fs::write(&path, b"old content").unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = WriteFile::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        // Without overwrite flag — should be blocked
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "new content"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.is_err(), "should be blocked without overwrite");

        // With overwrite flag — should succeed
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "new content",
            "overwrite": true
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "new content");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn create_directory_automatically() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = WriteFile::new(work_dir.clone(), checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let nested = work_dir.join("subdir").join("nested").join("file.txt");
        let args = serde_json::json!({
            "path": nested.to_str().unwrap(),
            "content": "nested content"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(nested.exists(), "file should have been created");
        let _ = std::fs::remove_dir_all(work_dir.join("subdir"));
    }

    #[tokio::test]
    async fn path_outside_workspace() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = WriteFile::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": "/tmp/outside.txt",
            "content": "test"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("outside"));
    }

    #[tokio::test]
    async fn checkpoint_on_overwrite() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let path = work_dir.join("checkpoint_test.txt");
        std::fs::write(&path, b"original").unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = WriteFile::new(work_dir, checkpoint.clone(), vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "content": "modified",
            "overwrite": true
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());

        let snapshot = checkpoint.get_snapshot(path.to_str().unwrap());
        assert!(snapshot.is_some(), "checkpoint should exist");
        assert_eq!(snapshot.unwrap().1, "original");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn schema_is_valid_json() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = WriteFile::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));
        let schema = tool.schema();
        assert!(schema.is_object());
        assert_eq!(schema["type"], "object");
        assert!(schema["required"].as_array().unwrap().contains(&Value::String("content".into())));
    }

    #[test]
    fn normalize_path_handles_dotdot() {
        let path = std::path::Path::new("/a/b/../c/./d");
        let normalized = normalize_path(path);
        assert_eq!(normalized, std::path::PathBuf::from("/a/c/d"));
    }
}
