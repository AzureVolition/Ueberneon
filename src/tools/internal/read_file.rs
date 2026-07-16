// read_file 工具 —— 读取文本文件并自动检测编码。
//
// 编码检测级联：BOM → 严格 UTF-8 → GB18030 → 有损 UTF-8，
// 与 v1 的 encoding.rs 保持一致，使得含 CJK 的 Windows 文件
// 可正常编辑而不会静默损坏其字节。

use std::path::PathBuf;
use std::sync::Arc;

use crate::agent::{Tool, ToolContext, ToolResult};
#[cfg(test)]
use crate::agent::{AgentMode, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;

use super::common::encoding;
use crate::tools::content_tracker::FileObserveTracker;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

///  read file tool
///  Read a text file from the local filesystem. 
///  Supports auto-detection of UTF-8, UTF-8 BOM, UTF-16 LE/BE (with and without BOM), 
///  and GB18030 (Chinese national standard). 
///  Use `offset` and `limit` to page through large files. 
///  The returned content includes line numbers.
#[derive(ToolMetaImpl)]
pub struct ReadFile {
    schema: Value ,
    read_only: bool,
    /// 工作目录 —— 所有文件路径必须在此目录之下。
    work_dir: PathBuf,
    /// 文件内容追踪器（陈旧锚点检查）。
    tracker: Arc<FileObserveTracker>,
}

impl ReadFile {
    pub fn new(work_dir: PathBuf, tracker: Arc<FileObserveTracker>) -> Self {
        Self {
            schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "File path to read"
                        },
                        "offset": {
                            "type": "integer",
                            "description": "0-based line offset to start reading from (default 0)",
                            "minimum": 0
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Maximum lines to return (default 2000, max 100000)",
                            "minimum": 1,
                            "maximum": 100000
                        },
                        "head": {
                            "type": "integer",
                            "description": "If provided, returns only the first N lines of the file (overrides offset)",
                            "minimum": 1
                        },
                        "tail": {
                            "type": "integer",
                            "description": "If provided, returns only the last N lines of the file (overrides offset and head)",
                            "minimum": 1
                        }
                    },
                    "required": ["path"]
                }),
            read_only: true,
            work_dir,
            tracker,
        }
    }

    /// 解析文件路径：相对路径拼接到 work_dir 下，绝对路径必须在 work_dir 内
    fn resolve_path(&self, path: &str) -> Result<PathBuf, String> {
        let p = std::path::Path::new(path);
        let abs = if p.is_relative() {
            self.work_dir.join(p)
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


#[async_trait::async_trait]
impl Tool for ReadFile {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let path_str = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => p,
            None => return Err("read_file: missing required argument 'path'".into()),
        };

        let path = self.resolve_path(path_str)?;

        // 安全检查：拒绝访问 .git 目录和二进制文件（通过快速 BOM 检查）
        if path.components().any(|c| c.as_os_str() == ".git") {
            return Err("access to .git directory is not allowed".into());
        }

        // 读取原始字节
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(e) => return Err(format!("read_file: failed to read '{}': {}", path_str, e)),
        };

        // 空文件快速返回
        if data.is_empty() {
            return Ok(ToolResult::ok("(empty file)"));
        }

        // 快速 BOM 检测拒绝二进制（非文本）文件
        if let Some(enc) = encoding::detect_quick(&data) {
            match enc {
                encoding::Kind::UTF8BOM | encoding::Kind::UTF16LE | encoding::Kind::UTF16BE => {
                    // 这些是可接受的文本格式
                }
                _ => {}
            }
        } else {
            // 无 BOM：检查 NUL 字节判断是否可能为二进制
            // 允许 UTF-16 无 BOM 的合法 NUL 分布
            let nul_count = data.iter().filter(|&&b| b == 0).count();
            if nul_count > 0 && data.len() > 16 {
                // 检查是否可能是 BOM-less UTF-16
                let (detected, _) = encoding::detect(&data);
                match detected {
                    encoding::Kind::UTF16LENoBOM | encoding::Kind::UTF16BE | encoding::Kind::UTF16BENoBOM => {
                        // 合法文本格式，继续
                    }
                    encoding::Kind::LossyUTF8 => {
                        // 对于有 NUL 的 LossyUTF8，很可能是二进制文件
                        if nul_count as f64 > data.len() as f64 * 0.3 {
                            return Ok(ToolResult::ok(format!(
                                "(binary file, {} bytes, {} NUL bytes — use a hex viewer to inspect)",
                                data.len(),
                                nul_count
                            )));
                        }
                    }
                    _ => {
                        // UTF8/UTF8BOM/GB18030 — 继续
                    }
                }
            }
        }

        // 检测编码并解码为 UTF-8 字符串
        let (enc, _) = encoding::detect(&data);
        let content = encoding::decode(&data, enc);

        // 解析可选参数
        let offset = args.get("offset").and_then(|v| v.as_i64()).unwrap_or(0).max(0) as usize;
        let limit = args.get("limit").and_then(|v| v.as_i64())
            .map(|v| v.max(1).min(100_000) as usize)
            .unwrap_or(2000);

        // 处理 head 和 tail 参数
        let lines: Vec<&str> = content.lines().collect();
        let total_lines = lines.len();

        let (start, end) = if let Some(tail) = args.get("tail").and_then(|v| v.as_i64()) {
            let n = tail.max(1) as usize;
            if n >= total_lines {
                (0, total_lines)
            } else {
                (total_lines - n, total_lines)
            }
        } else if let Some(head) = args.get("head").and_then(|v| v.as_i64()) {
            let n = head.max(1) as usize;
            (0, n.min(total_lines))
        } else {
            let end = (offset + limit).min(total_lines);
            (offset, end)
        };

        let selected = &lines[start..end];

        // 构建带行号的输出
        let mut output = String::new();

        // 输出文件元信息
        output.push_str(&format!("─── {} — {} lines total", path_str, total_lines));
        if enc != encoding::Kind::UTF8 {
            output.push_str(&format!(", encoding: {:?}", enc));
        }
        if start > 0 || end < total_lines {
            output.push_str(&format!(" (showing lines {}-{})", start + 1, end));
        }
        output.push('\n');

        for (i, line) in selected.iter().enumerate() {
            let line_num = start + i + 1;
            output.push_str(&format!("{:>6}→{}\n", line_num, line));
        }

        // 如果内容被截断（超过 limit），添加提示
        if end < total_lines {
            output.push_str(&format!(
                "─── (showing lines {}-{} of {}, use offset={} to see more)\n",
                start + 1, end, total_lines, end
            ));
        }

        // 记录内容哈希用作陈旧锚点检查
        self.tracker.observe(path_str, &content);

        Ok(ToolResult::ok(output))
    }
}


#[async_trait::async_trait]
impl CheckableTool for ReadFile {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use crate::tools::content_tracker::FileObserveTracker;

    fn make_tracker() -> Arc<FileObserveTracker> {
        Arc::new(FileObserveTracker::new())
    }

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn create_temp_file(content: &[u8]) -> (std::path::PathBuf, std::fs::File) {
        let dir = std::env::temp_dir();
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = dir.join(format!("_test_read_file_{}_{}.txt", std::process::id(), id));
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content).unwrap();
        (path, file)
    }

    #[tokio::test]
    async fn read_utf8_file() {
        let content = b"hello\nworld\nthird line\n";
        let (path, _file) = create_temp_file(content);
        let tool = ReadFile::new(std::env::temp_dir(), make_tracker());
        let args = serde_json::json!({"path": path.to_str().unwrap()});
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("hello"));
        assert!(result.output().contains("world"));
        assert!(result.output().contains("third line"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_with_offset_limit() {
        let content = b"line1\nline2\nline3\nline4\nline5\n";
        let (path, _file) = create_temp_file(content);
        let tool = ReadFile::new(std::env::temp_dir(), make_tracker());
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "offset": 1,
            "limit": 2
        });
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("line2"));
        assert!(result.output().contains("line3"));
        assert!(!result.output().contains("line1"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_head() {
        let content = b"line1\nline2\nline3\nline4\nline5\n";
        let (path, _file) = create_temp_file(content);
        let tool = ReadFile::new(std::env::temp_dir(), make_tracker());
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "head": 2
        });
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("line1"));
        assert!(result.output().contains("line2"));
        assert!(!result.output().contains("line3"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_tail() {
        let content = b"line1\nline2\nline3\nline4\nline5\n";
        let (path, _file) = create_temp_file(content);
        let tool = ReadFile::new(std::env::temp_dir(), make_tracker());
        let args = serde_json::json!({
            "path": path.to_str().unwrap(),
            "tail": 2
        });
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        assert!(result.error().is_none());
        assert!(!result.output().contains("line1"));
        assert!(!result.output().contains("line2"));
        assert!(!result.output().contains("line3"));
        assert!(result.output().contains("line4"));
        assert!(result.output().contains("line5"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn reject_git_path() {
        let dir = std::env::temp_dir();
        let repo_dir = dir.join("_test_git_repo");
        let _ = std::fs::create_dir_all(repo_dir.join(".git"));
        let tool = ReadFile::new(dir, make_tracker());
        // 使用相对路径，resolve 后会拼到 work_dir 下
        let args = serde_json::json!({"path": "_test_git_repo/.git/config"});
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        let _ = std::fs::remove_dir_all(&repo_dir);
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn missing_path() {
        let tool = ReadFile::new(std::env::temp_dir(), make_tracker());
        let args = serde_json::json!({});
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        assert!(result.error().is_some());
    }

    #[tokio::test]
    async fn empty_file() {
        let (path, _file) = create_temp_file(b"");
        let tool = ReadFile::new(std::env::temp_dir(), make_tracker());
        let args = serde_json::json!({"path": path.to_str().unwrap()});
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("empty"));
        let _ = std::fs::remove_file(&path);
    }

    #[tokio::test]
    async fn read_utf16_le_file() {
        let content: Vec<u8> = vec![
            0xFF, 0xFE, // BOM
            b'h', 0x00, b'e', 0x00, b'l', 0x00, b'l', 0x00, b'o', 0x00,
            b'\n', 0x00,
            b'w', 0x00, b'o', 0x00, b'r', 0x00, b'l', 0x00, b'd', 0x00,
        ];
        let (path, _file) = create_temp_file(&content);
        let tool = ReadFile::new(std::env::temp_dir(), make_tracker());
        let args = serde_json::json!({"path": path.to_str().unwrap()});
        let result = tool.execute(&ToolContext {
            call_id: "test".into(),
            plan_mode: false,
            agent_mode: AgentMode::Ask,
            progress: None,
        }, &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("hello"), "output: {}", result.output());
        assert!(result.output().contains("world"), "output: {}", result.output());
        let _ = std::fs::remove_file(&path);
    }
}
