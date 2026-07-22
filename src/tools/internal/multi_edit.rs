// multi_edit 工具 —— 对单个文件原子性地批量编辑。
//
// 接收 path + edits 数组，每个 edit 包含 old_string、new_string 和可选 replace_all。
// 编辑在内存中顺序应用；仅在所有编辑都成功时才写入磁盘。

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{ActionMode, Tool, ToolContext, ToolResult};
use llm::tool::ToolMeta;
#[cfg(test)]
use crate::agent::{AgentHandler, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde::Deserialize;
use serde_json::Value;

use super::common::edit;
use super::common::encoding;
use crate::tools::content_tracker::FileObserveTracker;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::tools::snapshot::SnapshotStore;
use crate::tools::diff::{self, Kind as DiffKind};
use crate::permission::{Check, Decision, gate::PermissionChecked};

/// multi_edit — 对单个文件进行原子性批量替换。
///
/// `work_dir` 是工作目录的共享引用 —— 所有文件路径必须在此目录之下。
#[derive(ToolMetaImpl)]
#[tool(schema = r#"{"type":"object","properties":{"path":{"type":"string","description":"File path"},"edits":{"type":"array","minItems":1,"items":{"type":"object","properties":{"old_string":{"type":"string","description":"Text to find"},"new_string":{"type":"string","description":"Replacement text"},"replace_all":{"type":"boolean","description":"Replace all occurrences instead of just the first"}},"required":["old_string","new_string"]},"description":"Ordered list of edits to apply"}},"required":["path","edits"]}"#)]
pub struct MultiEdit {
    /// 工作目录（共享引用语义）。
    work_dir: PathBuf,
    /// 检查点存储（写前记录快照）。
    checkpoint: Arc<SnapshotStore>,
    /// 权限检查列表。
    checks: Vec<Box<dyn Check>>,
    /// 文件内容追踪器（陈旧锚点 + 循环守卫）。
    tracker: Arc<FileObserveTracker>,
}

/// 单次编辑操作。
#[derive(Debug, Deserialize)]
struct EditOp {
    /// 要查找的旧文本。
    old_string: String,
    /// 替换的新文本（可为空以删除）。
    new_string: String,
    /// 是否替换所有匹配（而非仅第一个）。
    #[serde(default)]
    replace_all: bool,
}

/// multi_edit 工具参数。
#[derive(Debug, Deserialize)]
struct MultiEditParams {
    /// 文件路径。
    path: String,
    /// 编辑操作列表（至少 1 项）。
    edits: Vec<EditOp>,
}

impl MultiEdit {
    pub fn new(work_dir: PathBuf, checkpoint: Arc<SnapshotStore>, checks: Vec<Box<dyn Check>>, tracker: Arc<FileObserveTracker>) -> Self {
        Self {
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
                Ok(c) => c,
                Err(_) => joined,
            }
        } else {
            p.to_path_buf()
        };

        if !abs.starts_with(&self.work_dir) {
            return Err(format!(
                "path '{}' is outside the workspace directory '{}'",
                abs.display(),
                self.work_dir.display()
            ));
        }

        Ok(abs)
    }
}

impl PermissionChecked for MultiEdit {
    fn permission_checks(&self) -> &[Box<dyn Check>] {
        &self.checks
    }
}

#[async_trait::async_trait]
impl Tool for MultiEdit {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        // 1. 解析参数
        let params: MultiEditParams = match serde_json::from_value(args.clone()) {
            Ok(p) => p,
            Err(e) => return Err(format!("multi_edit: invalid arguments: {e}")),
        };

        if params.edits.is_empty() {
            return Err("multi_edit: 'edits' must not be empty".into());
        }

        // 2. 解析路径
        let path = match self.resolve_path(&params.path) {
            Ok(p) => p,
            Err(e) => return Err(format!("multi_edit: {}", e)),
        };

        // 3. 读取文件
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => return Err(format!("multi_edit: failed to read '{}': {}", params.path, e)),
        };

        let (enc, _) = encoding::detect(&data);
        let mut content = encoding::decode(&data, enc);
        let original_content = content.clone();

        // 4. 陈旧锚点检查
        if let Err(msg) = self.tracker.check_anchor(&params.path, &content) {
            return Err(msg);
        }

        // 5. 循环守卫：检查每项编辑是否重复
        for (i, op) in params.edits.iter().enumerate() {
            if let Err(msg) = self.tracker.check_loop(&params.path, &op.old_string, &op.new_string) {
                return Err(format!("multi_edit: edit {} — {}", i, msg));
            }
        }

        // 6. 在内存中顺序应用所有编辑
        let mut _total_applied = 0usize;
        for (i, op) in params.edits.iter().enumerate() {
            if op.old_string.is_empty() {
                return Err(format!(
                    "multi_edit: edits[{}].old_string must not be empty", i
                ));
            }

            let result = edit::apply_edit(&content, &op.old_string, &op.new_string, op.replace_all);

            match result.applied {
                0 if result.matches == 0 => {
                    return Err(format!(
                        "multi_edit: edit {} failed — {}",
                        i,
                        edit::old_string_not_found_error(&params.path, &op.old_string, &content)
                    ));
                }
                0 if result.matches > 1 && !op.replace_all => {
                    return Err(format!(
                        "multi_edit: edit {} failed — {}",
                        i,
                        edit::old_string_not_unique_error(&params.path, &op.old_string, &content, result.matches)
                    ));
                }
                _ => {
                    _total_applied += result.applied;
                    content = result.updated;
                }
            }
        }

        // 5. 构建 diff 并记录 checkpoint
        let file_change = diff::build_diff(&params.path, &original_content, &content, DiffKind::Modify);
        self.checkpoint.snapshot(&params.path, &original_content, 0);

        // 6. 回写文件
        let output_bytes = encoding::encode(&content, enc);
        if let Err(e) = std::fs::write(&path, &output_bytes) {
            return Err(format!("multi_edit: failed to write '{}': {}", params.path, e));
        }

        // 7. 更新追踪器
        self.tracker.record_write(&params.path, &content);
        for op in &params.edits {
            self.tracker.record_edit(&params.path, &op.old_string, &op.new_string);
        }

        // 8. 返回成功消息
        let summary = diff::change_summary(&file_change);
        Ok(ToolResult::ok(format!("edited {} ({} edits applied)\n{}", params.path, params.edits.len(), summary)))
    }
}

#[async_trait::async_trait]
impl CheckableTool for MultiEdit {
    fn check(&self, ctx: &ToolContext, args: &Value) -> Decision {
        match self.check_permission(self.name(), args, *ctx.handler.agent_mode.lock().expect("agent_mode lock poisoned")) {
            Decision::Allow => {}
            decision => return decision,
        }
        match ctx.plan_mode {
            ActionMode::Plan => return Decision::Deny("multi_edit is not allowed in plan mode".into()),
            ActionMode::Regular => {}
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir();
        let id = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        dir.join(format!("_test_multi_edit_{}_{}", std::process::id(), id))
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            handler: AgentHandler::default(),
            progress: None,
            main_conversation_id: String::new(),
            project_id: None,
        cancel_token: None,
        }
    }

    #[tokio::test]
    async fn multiple_edits() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let path = work_dir.join("test.txt");
        std::fs::write(&path, b"a\nb\nc\n").unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = MultiEdit::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "edits": [
                {"old_string": "a", "new_string": "x"},
                {"old_string": "c", "new_string": "z"}
            ]
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "x\nb\nz\n");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn edit_failure_does_not_write() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let path = work_dir.join("test.txt");
        std::fs::write(&path, b"a\nb\nc\n").unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = MultiEdit::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "edits": [
                {"old_string": "a", "new_string": "x"},
                {"old_string": "nonexistent", "new_string": "y"}
            ]
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some(), "should have failed");

        // File should remain unchanged
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "a\nb\nc\n", "file should not have been modified");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn replace_all_support() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let path = work_dir.join("test.txt");
        std::fs::write(&path, b"x\ny\nx\ny\n").unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = MultiEdit::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "edits": [
                {"old_string": "x", "new_string": "a", "replace_all": true}
            ]
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());

        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "a\ny\na\ny\n");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn path_outside_workspace() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = MultiEdit::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": "/etc/passwd",
            "edits": [{"old_string": "root", "new_string": "user"}]
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("outside"));
    }
}
