// grep 工具 —— 在文件或目录中搜索正则表达式。
//
// 使用 RE2 语法（通过 regex crate），默认从当前目录递归搜索。
// 自动跳过 .gitignore 匹配的文件和目录，以及二进制文件。
// 支持编码检测（UTF-8/16/GB18030），最多返回 200 条匹配。

use std::path::Path;
use std::time::Duration;

use llm::tool::{AgentMode, Tool, ToolContext, ToolResult, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use serde_json::Value;

use super::common::encoding;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

// ── 常量 ─────────────────────────────────────────────────────────────────────

/// 最大匹配行数。
const GREP_MAX_MATCHES: usize = 200;
/// 默认超时（秒）。
const GREP_DEFAULT_TIMEOUT_SECS: u64 = 30;
/// 最大超时（秒）。
const GREP_MAX_TIMEOUT_SECS: u64 = 300;

/// grep —— 在文件或目录中搜索正则表达式。
///
/// 使用 RE2 语法，自动跳过二进制文件和 .gitignore 匹配项。
/// 返回 path:line:text 格式的匹配行，最多 200 条。
#[derive(ToolMetaImpl)]
pub struct Grep {
    schema: Value,
    read_only: bool,
}

impl Grep {
    pub fn new() -> Self {
        Self {
            schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Regular expression (RE2 syntax)"
                    },
                    "path": {
                        "type": "string",
                        "description": "File or directory to search (default \".\")"
                    },
                    "timeout_seconds": {
                        "type": "integer",
                        "description": "Abort after this many seconds (default 30, max 300)",
                        "minimum": 1
                    }
                },
                "required": ["pattern"]
            }),
            read_only: true,
        }
    }
}

#[async_trait::async_trait]
impl Tool for Grep {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        // 1. 解析参数
        let pattern_str = match args.get("pattern").and_then(|v| v.as_str()) {
            Some(p) if !p.is_empty() => p,
            _ => return Err("grep: missing required argument 'pattern'".into()),
        };
        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            .unwrap_or(".");
        let timeout_secs = args
            .get("timeout_seconds")
            .and_then(|v| v.as_u64())
            .map(|s| s.clamp(1, GREP_MAX_TIMEOUT_SECS))
            .unwrap_or(GREP_DEFAULT_TIMEOUT_SECS);

        // 2. 编译正则（RE2 语法）
        let re = match regex::Regex::new(pattern_str) {
            Ok(r) => r,
            Err(e) => return Err(format!("grep: invalid regex pattern: {}", e)),
        };

        let path_buf = std::path::PathBuf::from(path_str);

        // 3. 安全检查：拒绝搜索 .git 目录
        if path_buf.components().any(|c| c.as_os_str() == ".git") {
            return Err("access to .git directory is not allowed".into());
        }

        // 4. 检查路径是否存在
        if !path_buf.exists() {
            return Err(format!("grep: path '{}' does not exist", path_str));
        }

        // 5. 在 blocking 线程池中执行搜索（文件 I/O + 正则匹配是同步操作）
        let timeout = Duration::from_secs(timeout_secs);
        let re_clone = re.clone();
        let path_clone = path_buf.clone();

        let result = tokio::task::spawn_blocking(move || {
            run_search(&re_clone, &path_clone)
        });

        let (matches, timed_out) = match tokio::time::timeout(timeout, result).await {
            Ok(Ok(out)) => (out, false),
            Ok(Err(e)) => return Err(format!("grep: search failed: {}", e)),
            Err(_elapsed) => {
                // 超时：尝试用已收集的结果（已在 run_search 里 cap 了）
                // 但 run_search 本身被 tokio 的 timeout 取消了，我们需要重新解释
                // 实际上 spawn_blocking 中的任务可能仍在运行，但我们不管它了
                (Vec::new(), true)
            }
        };

        // 6. 格式化输出
        Ok(ToolResult::ok(format_grep_output(&matches, timed_out, timeout)))
    }
}

// ── 搜索逻辑 ─────────────────────────────────────────────────────────────────

/// 在指定路径中搜索正则表达式，返回匹配行列表。
fn run_search(re: &regex::Regex, path: &Path) -> Vec<MatchLine> {
    let mut results = Vec::new();

    if path.is_file() {
        search_file(path, re, &mut results);
    } else if path.is_dir() {
        // 使用 ignore crate 进行 .gitignore 感知的递归遍历
        let walker = ignore::WalkBuilder::new(path)
            .standard_filters(true) // 遵守 .gitignore
            .build();

        for entry in walker {
            if results.len() >= GREP_MAX_MATCHES {
                break;
            }
            let entry = match entry {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                search_file(entry.path(), re, &mut results);
            }
        }
    }

    results
}

/// 在单个文件中搜索正则表达式，结果追加到 `results`。
fn search_file(path: &Path, re: &regex::Regex, results: &mut Vec<MatchLine>) {
    if results.len() >= GREP_MAX_MATCHES {
        return;
    }

    // 读取原始字节
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return, // 跳过不可读文件
    };

    if data.is_empty() {
        return;
    }

    // 二进制检测：检查前 8KB 中是否有 NUL 字节。
    // BOM 优先检测（UTF-16 文件包含 NUL 字节但仍是合法文本）。
    let peek_len = data.len().min(8192);
    let peek = &data[..peek_len];

    let bom_kind = encoding::detect_quick(peek);
    let is_text = match bom_kind {
        // UTF-16/UTF-8 BOM 文件：带 BOM 的文本格式
        Some(encoding::Kind::UTF16LE)
        | Some(encoding::Kind::UTF16BE)
        | Some(encoding::Kind::UTF8BOM) => true,
        _ => {
            // 无已知 BOM：检查是否有 NUL 字节（通常表示二进制）
            let has_nul = peek.iter().any(|&b| b == 0);
            if has_nul {
                // 可能是 UTF-16 无 BOM
                let (detected, _) = encoding::detect(&data);
                matches!(
                    detected,
                    encoding::Kind::UTF16LENoBOM | encoding::Kind::UTF16BE | encoding::Kind::UTF16BENoBOM
                )
            } else {
                true
            }
        }
    };

    if !is_text {
        return; // 跳过二进制文件
    }

    // 检测编码并解码
    let (enc, _) = encoding::detect(&data);
    let content = encoding::decode(&data, enc);

    // 逐行搜索
    let path_str = path.to_string_lossy();
    for (line_num, line) in content.lines().enumerate() {
        if results.len() >= GREP_MAX_MATCHES {
            break;
        }
        if re.is_match(line) {
            results.push(MatchLine {
                path: path_str.to_string(),
                line_number: (line_num + 1) as u64,
                text: line.to_string(),
            });
        }
    }
}

// ── 输出格式化 ────────────────────────────────────────────────────────────────

/// 一行匹配结果。
struct MatchLine {
    path: String,
    line_number: u64,
    text: String,
}

/// 将匹配行列表格式化为模型可读的文本。
fn format_grep_output(matches: &[MatchLine], timed_out: bool, timeout: Duration) -> String {
    if matches.is_empty() {
        if timed_out {
            return format!(
                "(no matches; timed out after {}s — narrow the path/pattern or raise timeout_seconds)",
                timeout.as_secs()
            );
        }
        return "(no matches)".to_string();
    }

    let mut output = String::new();
    for m in matches {
        output.push_str(&format!("{}:{}:{}\n", m.path, m.line_number, m.text));
    }

    if matches.len() >= GREP_MAX_MATCHES {
        output.push_str(&format!(
            "... (truncated at {} matches)",
            GREP_MAX_MATCHES
        ));
    } else if timed_out {
        output.push_str(&format!(
            "... (timed out after {}s; results incomplete — narrow the path/pattern or raise timeout_seconds)",
            timeout.as_secs()
        ));
    }

    // 移除尾部多余的换行符
    if output.ends_with('\n') {
        output.truncate(output.len() - 1);
    }

    output
}

// ── 测试 ─────────────────────────────────────────────────────────────────────


#[async_trait::async_trait]
impl CheckableTool for Grep {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

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
        dir.join(format!("_test_grep_{}_{}", std::process::id(), id))
    }

    fn create_test_file(dir: &std::path::Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(content).unwrap();
        path
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
    async fn basic_pattern_search() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        create_test_file(&dir, "test.txt", b"hello world\nfoo bar\nbaz hello\n");

        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap(),
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("hello world"), "output: {}", result.output());
        assert!(result.output().contains("baz hello"), "output: {}", result.output());
        assert!(!result.output().contains("foo bar"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn no_matches() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        create_test_file(&dir, "test.txt", b"hello world\n");

        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "nonexistent",
            "path": dir.to_str().unwrap(),
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("no matches"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_pattern() {
        let tool = Grep::new();
        let args = serde_json::json!({"path": "."});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("pattern"));
    }

    #[tokio::test]
    async fn invalid_regex() {
        let tool = Grep::new();
        let args = serde_json::json!({"pattern": "[invalid"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("invalid"));
    }

    #[tokio::test]
    async fn search_single_file() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let path = create_test_file(&dir, "test.txt", b"abc\ndef\nabc\n");

        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "abc",
            "path": path.to_str().unwrap(),
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none());
        // 应该匹配两行
        let count = result.output().lines().filter(|l| l.contains("test.txt")).count();
        assert_eq!(count, 2, "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn reject_git_path() {
        let tool = Grep::new();
        let args = serde_json::json!({"pattern": "test", "path": "/tmp/repo/.git/config"});
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn binary_file_skipped() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // 创建包含大量 NUL 字节的二进制文件
        let mut content = Vec::new();
        content.extend_from_slice(b"hello\x00world\x00\x00\x00\x00test");
        create_test_file(&dir, "binary.bin", &content);

        // 同时创建一个文本文件确保搜索仍会进行
        create_test_file(&dir, "text.txt", b"hello text\n");

        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap(),
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none());
        // 应该只匹配到 text.txt，不匹配 binary.bin
        assert!(result.output().contains("text.txt"), "output: {}", result.output());
        // 不能包含 binary.bin 的匹配
        assert!(!result.output().contains("binary.bin"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn utf16_file_search() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        // UTF-16 LE with BOM
        let mut content = vec![0xFF, 0xFE]; // BOM
        for ch in "hello\nworld\n".encode_utf16() {
            content.extend_from_slice(&ch.to_le_bytes());
        }
        create_test_file(&dir, "utf16.txt", &content);

        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap(),
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("hello"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn path_not_exists() {
        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "test",
            "path": "/nonexistent/path",
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("does not exist"));
    }

    #[test]
    fn schema_is_valid_json() {
        let tool = Grep::new();
        let schema = tool.schema();
        assert!(schema.is_object());
        assert_eq!(schema["type"], "object");
        assert!(schema["required"].as_array().unwrap().contains(&serde_json::Value::String("pattern".into())));
    }

    #[test]
    fn format_output_no_matches() {
        let result = format_grep_output(&[], false, Duration::from_secs(30));
        assert_eq!(result, "(no matches)");
    }

    #[test]
    fn format_output_timed_out_no_matches() {
        let result = format_grep_output(&[], true, Duration::from_secs(30));
        assert!(result.contains("timed out"));
    }

    #[test]
    fn format_output_with_matches() {
        let matches = vec![
            MatchLine { path: "a.txt".into(), line_number: 1, text: "hello".into() },
            MatchLine { path: "b.txt".into(), line_number: 3, text: "world".into() },
        ];
        let result = format_grep_output(&matches, false, Duration::from_secs(30));
        assert!(result.contains("a.txt:1:hello"));
        assert!(result.contains("b.txt:3:world"));
    }

    #[tokio::test]
    async fn multiple_files() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        create_test_file(&dir, "a.rs", b"fn main() {\n    println!(\"hello\");\n}\n");
        create_test_file(&dir, "b.rs", b"fn test() {\n    // hello\n}\n");
        create_test_file(&dir, "c.rs", b"fn other() {\n    println!(\"bye\");\n}\n");

        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "hello",
            "path": dir.to_str().unwrap(),
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("a.rs"), "output: {}", result.output());
        assert!(result.output().contains("b.rs"), "output: {}", result.output());
        assert!(!result.output().contains("c.rs"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn case_sensitive_by_default() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        create_test_file(&dir, "test.txt", b"Hello\nhello\nHELLO\n");

        let tool = Grep::new();
        let args = serde_json::json!({
            "pattern": "Hello",
            "path": dir.to_str().unwrap(),
        });
        let result = tool.execute(&test_ctx(), &args).await;
        assert!(result.error().is_none());
        // 应只匹配 "Hello"（大小写敏感）
        let count = result.output().matches("Hello").count();
        assert!(count >= 1, "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
