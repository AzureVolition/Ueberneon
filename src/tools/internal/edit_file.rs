// edit_file 工具 —— 在文件中精确替换一段文本。
//
// 接收 path、old_string、new_string，在文件中找到唯一匹配并替换。
// 匹配策略：精确匹配 → CRLF 归一化 → 模糊匹配（行号前缀剥离等）。
// 支持编码保留（UTF-8/16/GB18030 无损往返）。写前通过 checkpoint 记录快照。

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::agent::{ActionMode, Tool, ToolContext, ToolResult};
use llm::tool::ToolMeta;
#[cfg(test)]
use crate::agent::{AgentMode, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;
use crate::permission::{Check, Decision, gate::PermissionChecked};
use crate::tools::internal::common::checkable_tool::CheckableTool;

use super::common::edit;
use super::common::encoding;
use crate::tools::content_tracker::FileObserveTracker;
use crate::tools::snapshot::SnapshotStore;
use crate::tools::diff::{self, Kind as DiffKind};

// re-export for registry convenience
pub use crate::tools::diff::FileChange;

/// edit_file — 用精确字符串替换编辑文件。
///
/// old_string 必须在文件中唯一出现；添加周围上下文以消歧。
/// 用于定向编辑而非重写整个文件。
///
/// `work_dir` 是工作目录的共享引用 —— 所有文件路径必须在此目录之下，
/// 相对路径会相对于 work_dir 解析。
#[derive(ToolMetaImpl)]
pub struct EditFile {
    schema: Value,
    read_only: bool,
    /// 工作目录（共享引用语义）。路径限制 + 相对路径解析的基础。
    work_dir: PathBuf,
    /// 检查点存储（写前记录快照）。
    checkpoint: Arc<SnapshotStore>,
    /// 权限检查列表（可复用 Check 组合）。
    checks: Vec<Box<dyn Check>>,
    /// 文件内容追踪器（陈旧锚点 + 循环守卫）。
    tracker: Arc<FileObserveTracker>,
}

impl EditFile {
    pub fn new(work_dir: PathBuf, checkpoint: Arc<SnapshotStore>, checks: Vec<Box<dyn Check>>, tracker: Arc<FileObserveTracker>) -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "Exact text to replace (must be unique in the file)"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "Replacement text (may be empty to delete)"
                    }
                },
                "required": ["path", "old_string", "new_string"]
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
    /// - 不存在的文件尝试拼接后做路径规范检查
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

impl PermissionChecked for EditFile {
    fn permission_checks(&self) -> &[Box<dyn Check>] {
        &self.checks
    }
}

#[async_trait::async_trait]
impl Tool for EditFile {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {

        // 1. 解析参数
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return Err("edit_file: missing required argument 'path'".into()),
        };
        let old_string = match args.get("old_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return Err("edit_file: missing required argument 'old_string'".into()),
        };
        let new_string = match args.get("new_string").and_then(|v| v.as_str()) {
            Some(s) => s,
            None => return Err("edit_file: missing required argument 'new_string'".into()),
        };

        if old_string.is_empty() {
            return Err("edit_file: 'old_string' must not be empty".into());
        }

        // 2. 解析路径并检查范围
        let path = match self.resolve_path(path_str) {
            Ok(p) => p,
            Err(e) => return Err(format!("edit_file: {}", e)),
        };

        // 3. 读取文件
        let data = match std::fs::read(&path) {
            Ok(d) => d,
            Err(e) => return Err(format!("edit_file: failed to read '{}': {}", path_str, e)),
        };

        let (enc, _) = encoding::detect(&data);
        let content = encoding::decode(&data, enc);

        // 4. 陈旧锚点检查：文件内容自上次读取后是否被外部修改
        if let Err(msg) = self.tracker.check_anchor(path_str, &content) {
            return Err(msg);
        }

        // 5. 循环守卫：检测重复编辑
        if let Err(msg) = self.tracker.check_loop(path_str, old_string, new_string) {
            return Err(msg);
        }

        // 6. 应用编辑
        let result = edit::apply_edit(&content, old_string, new_string, false);

        match result.applied {
            0 if result.matches == 0 => {
                return Err(edit::old_string_not_found_error(path_str, old_string, &content));
            }
            0 if result.matches > 1 => {
                return Err(edit::old_string_not_unique_error(path_str, old_string, &content, result.matches));
            }
            _ => {}
        }

        // 5. 构建 diff
        let file_change = diff::build_diff(path_str, &content, &result.updated, DiffKind::Modify);

        // 6. 记录 checkpoint 快照（写前）
        self.checkpoint.snapshot(path_str, &content, 0);

        // 7. 回写文件（保持原始编码）
        let output_bytes = encoding::encode(&result.updated, enc);
        if let Err(e) = std::fs::write(&path, &output_bytes) {
            return Err(format!("edit_file: failed to write '{}': {}", path_str, e));
        }

        // 8. 更新追踪器
        self.tracker.record_write(path_str, &result.updated);
        self.tracker.record_edit(path_str, old_string, new_string);

        // 9. 返回成功消息
        let fuzzy_suffix = if result.fuzzy { " (fuzzy match)" } else { "" };
        let summary = diff::change_summary(&file_change);
        let diff_text = &file_change.unified_diff;
        Ok(ToolResult::ok(format!("edited {}{}\n{}\n\n{}", path_str, fuzzy_suffix, summary, diff_text)))
    }
}

#[async_trait::async_trait]
impl CheckableTool for EditFile {
    fn check(&self, ctx: &ToolContext, args: &Value) -> Decision {
        match self.check_permission(self.name(), args, *ctx.agent_mode.lock().unwrap()) {
            Decision::Allow => {}
            decision => return decision,
        }
        match ctx.plan_mode {
            ActionMode::Plan => return Decision::Deny("edit_file is not allowed in plan mode".into()),
            ActionMode::Regular => {}
        }
        Decision::Allow
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use llm::tool::ToolMeta;

    static TEST_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir();
        let id = TEST_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        dir.join(format!("_test_edit_file_{}_{}", std::process::id(), id))
    }

    fn setup_test(content: &[u8], filename: &str) -> (PathBuf, EditFile) {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let path = work_dir.join(filename);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content).unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tracker = Arc::new(FileObserveTracker::new());
        // 模拟 read_file 后的 observe
        if !content.is_empty() {
            let decoded = String::from_utf8_lossy(content);
            tracker.observe(&path.to_string_lossy(), &decoded);
        }
        let tool = EditFile::new(work_dir.clone(), checkpoint, vec![], tracker);
        (path, tool)
    }

    fn test_ctx() -> ToolContext {
        ToolContext {
            call_id: "test".into(),
            plan_mode: ActionMode::Regular,
            agent_mode: Arc::new(Mutex::new(AgentMode::Ask)),
            progress: None,
        }
    }

    #[tokio::test]
    async fn basic_replace() {
        let (path, tool) = setup_test(b"hello\nworld\n", "test.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "old_string": "world",
            "new_string": "there"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("edited"), "output: {}", result.output());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello\nthere\n");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn not_found_error() {
        let (path, tool) = setup_test(b"hello\nworld\n", "test.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "old_string": "nonexistent",
            "new_string": "replacement"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("not found"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn not_unique_error() {
        let (path, tool) = setup_test(b"hello\nhello\n", "test.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "old_string": "hello",
            "new_string": "hi"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("not unique"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn delete_text() {
        let (path, tool) = setup_test(b"hello\nworld\nend\n", "test.txt");
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "old_string": "world\n",
            "new_string": ""
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none());
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content, "hello\nend\n");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn checkpoint_recorded() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let path = work_dir.join("test.txt");
        std::fs::write(&path, b"original content\n").unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = EditFile::new(work_dir, checkpoint.clone(), vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "old_string": "original content",
            "new_string": "modified content"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none());

        let snapshot = checkpoint.get_snapshot(path.to_str().unwrap());
        assert!(snapshot.is_some());
        assert_eq!(snapshot.unwrap().1, "original content\n");
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn path_outside_workspace() {
        let work_dir = temp_dir();
        std::fs::create_dir_all(&work_dir).unwrap();
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = EditFile::new(work_dir, checkpoint, vec![], Arc::new(FileObserveTracker::new()));

        let args = serde_json::json!({
            "path": "/etc/passwd",
            "old_string": "root",
            "new_string": "user"
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("outside"));
    }

    #[test]
    fn schema_is_valid_json() {
        let checkpoint = Arc::new(SnapshotStore::new());
        let tool = EditFile::new(PathBuf::from("/tmp"), checkpoint, vec![], Arc::new(FileObserveTracker::new()));
        let schema = tool.schema();
        assert!(schema.is_object());
        assert_eq!(schema["type"], "object");
    }
}
