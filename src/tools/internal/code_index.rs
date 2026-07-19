// code_index 工具 —— 代码符号索引。
//
// 使用正则表达式从源文件中提取函数、结构体、类等符号定义。
// 支持 outline（列出路径下所有符号）和 search（按名搜索）两种模式。

use std::path::PathBuf;
use std::path::Path;

use crate::agent::{Tool, ToolContext, ToolResult};
#[cfg(test)]
use crate::agent::{AgentMode, ActionMode, ToolResultExt};
use racpagent_macros::ToolMetaImpl;
use regex::Regex;
use serde_json::Value;
use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::permission::Decision;

/// code_index — 提取源文件中的符号定义。
///
/// action="outline" 列出路径下所有符号；
/// action="search" 按名称搜索符号。
/// 支持 .rs/.py/.js/.ts/.go/.java/.c/.cpp/.h/.cs/.kt 文件。
#[derive(ToolMetaImpl)]
#[tool(read_only)]
#[tool(schema = r#"{"type":"object","properties":{"action":{"type":"string","description":"Action: 'outline' (list symbols under path) or 'search' (search by name)","enum":["outline","search"]},"path":{"type":"string","description":"File or directory path (default ".")"},"query":{"type":"string","description":"Symbol name or substring to search for (required for action=search)"},"kind":{"type":"string","description":"Filter by symbol kind: func/fn, method, class, type, interface, const, var, struct, enum, trait, mod, impl"},"limit":{"type":"integer","description":"Maximum symbols to return (default 100, max 200)","minimum":1}},"required":["action"]}"#)]
pub struct CodeIndex {
    work_dir: PathBuf,
}

/// 一个符号定义。
#[derive(Debug, Clone)]
struct CodeSymbol {
    file: String,
    line: usize,
    kind: String,
    name: String,
    parent: Option<String>,
    signature: String,
}

/// 文件扩展名到符号解析器的映射。
type SymbolParser = fn(&str, &str, &Regex, &Regex) -> Vec<CodeSymbol>;

/// 支持的文件类型及其解析器。
struct LangDef {
    extensions: &'static [&'static str],
    parser: SymbolParser,
    kinds: &'static [&'static str],
}

const CODE_INDEX_DEFAULT_LIMIT: usize = 100;
const CODE_INDEX_MAX_LIMIT: usize = 200;
const CODE_INDEX_MAX_FILES: usize = 2000;
const CODE_INDEX_MAX_FILE_SIZE: u64 = 1_048_576; // 1 MiB

/// 所有支持的语言定义。
const LANGUAGES: &[LangDef] = &[
    LangDef {
        extensions: &["rs"],
        parser: parse_rust,
        kinds: &["fn", "struct", "enum", "trait", "impl", "mod", "type", "const", "macro"],
    },
    LangDef {
        extensions: &["py"],
        parser: parse_python,
        kinds: &["class", "function"],
    },
    LangDef {
        extensions: &["js", "jsx", "ts", "tsx"],
        parser: parse_jsts,
        kinds: &["function", "class", "interface", "type", "enum", "const", "var"],
    },
    LangDef {
        extensions: &["go"],
        parser: parse_go,
        kinds: &["func", "type", "struct", "interface"],
    },
    LangDef {
        extensions: &["java"],
        parser: parse_java,
        kinds: &["class", "interface", "enum", "record", "method"],
    },
    LangDef {
        extensions: &["c", "cc", "cpp", "h", "hpp"],
        parser: parse_c_cpp,
        kinds: &["class", "struct", "enum", "function"],
    },
    LangDef {
        extensions: &["cs"],
        parser: parse_csharp,
        kinds: &["class", "interface", "struct", "enum", "record"],
    },
    LangDef {
        extensions: &["kt", "kts"],
        parser: parse_kotlin,
        kinds: &["fun", "class", "interface", "object", "enum"],
    },
];

impl CodeIndex {
    pub fn new(work_dir: PathBuf) -> Self {
        Self {
            work_dir,
        }
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf, String> {
        let p = Path::new(path);
        let abs = if p.is_relative() { self.work_dir.join(p) } else { p.to_path_buf() };
        if !abs.starts_with(&self.work_dir) {
            return Err(format!("path '{}' is outside workspace '{}'", abs.display(), self.work_dir.display()));
        }
        Ok(abs)
    }

    /// 根据文件扩展名查找对应的语言定义。
    fn lang_for_ext(ext: &str) -> Option<&'static LangDef> {
        LANGUAGES.iter().find(|lang| lang.extensions.contains(&ext))
    }

    /// 从文件中提取符号。
    fn parse_file(path: &Path, lang: &LangDef) -> Vec<CodeSymbol> {
        let data = match std::fs::read(path) {
            Ok(d) => d,
            Err(_) => return Vec::new(),
        };

        if data.is_empty() || data.len() as u64 > CODE_INDEX_MAX_FILE_SIZE {
            return Vec::new();
        }

        // 尝试 UTF-8 解码
        let content = match String::from_utf8(data) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let file_path = path.to_string_lossy().to_string();

        // 准备通用正则
        let re_leading_ws = Regex::new(r"^\s*").unwrap();
        let re_trailing = Regex::new(r"\s*$").unwrap();

        (lang.parser)(&file_path, &content, &re_leading_ws, &re_trailing)
    }
}

// ── Rust 解析器 ───────────────────────────────────────────────────────────

fn parse_rust(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_fn = Regex::new(r"^\s*(?:pub\s+)?(?:async\s+)?fn\s+(\w+)").unwrap();
    let re_item = Regex::new(r"^\s*(?:pub\s+)?(struct|enum|trait|union|type|mod)\s+(\w+)").unwrap();
    let re_impl = Regex::new(r"^\s*(?:pub\s+)?(?:unsafe\s+)?impl\s*(?:<[^>]+>)?\s+(\w+(?:\s+for\s+\w+)?)").unwrap();
    let re_const = Regex::new(r"^\s*(?:pub\s+)?const\s+(\w+)").unwrap();
    let re_macro = Regex::new(r"^\s*macro_rules!\s*(\w+)").unwrap();

    for (i, line) in lines.iter().enumerate() {
        let line_num = i + 1;
        let trimmed = line.trim_start();

        if let Some(cap) = re_fn.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: line_num,
                kind: "fn".into(), name: cap[1].to_string(),
                parent: None, signature: trimmed.to_string(),
            });
        } else if let Some(cap) = re_item.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: line_num,
                kind: cap[1].to_string(), name: cap[2].to_string(),
                parent: None, signature: trimmed.to_string(),
            });
        } else if let Some(cap) = re_impl.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: line_num,
                kind: "impl".into(), name: cap[1].to_string(),
                parent: None, signature: trimmed.to_string(),
            });
        } else if let Some(cap) = re_const.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: line_num,
                kind: "const".into(), name: cap[1].to_string(),
                parent: None, signature: trimmed.to_string(),
            });
        } else if let Some(cap) = re_macro.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: line_num,
                kind: "macro".into(), name: cap[1].to_string(),
                parent: None, signature: trimmed.to_string(),
            });
        }
    }

    symbols
}

// ── Python 解析器 ────────────────────────────────────────────────────────

fn parse_python(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_class = Regex::new(r"^\s*class\s+(\w+)").unwrap();
    let re_fn = Regex::new(r"^\s*(?:async\s+)?def\s+(\w+)").unwrap();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_fn.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "function".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_class.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "class".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        }
    }

    symbols
}

// ── JS/TS 解析器 ────────────────────────────────────────────────────────

fn parse_jsts(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_fn = Regex::new(r"^\s*(?:export\s+)?(?:async\s+)?function\s+(\w+)").unwrap();
    let re_class = Regex::new(r"^\s*(?:export\s+)?(?:abstract\s+)?class\s+(\w+)").unwrap();
    let re_interface = Regex::new(r"^\s*(?:export\s+)?interface\s+(\w+)").unwrap();
    let re_type = Regex::new(r"^\s*(?:export\s+)?type\s+(\w+)").unwrap();
    let re_enum = Regex::new(r"^\s*(?:export\s+)?enum\s+(\w+)").unwrap();
    let re_const = Regex::new(r"^\s*(?:export\s+)?(?:const|let|var)\s+(\w+)\s*=").unwrap();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_fn.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "function".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_class.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "class".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_interface.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "interface".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_type.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "type".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_enum.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "enum".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_const.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "const".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        }
    }

    symbols
}

// ── Go 解析器 ────────────────────────────────────────────────────────────

fn parse_go(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_func = Regex::new(r"^\s*func\s+(?:\([^)]*\)\s+)?(\w+)").unwrap();
    let re_type = Regex::new(r"^\s*type\s+(\w+)\s+(struct|interface)").unwrap();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_func.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "func".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_type.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: cap[2].to_string(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        }
    }

    symbols
}

// ── Java 解析器 ─────────────────────────────────────────────────────────

fn parse_java(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_class = Regex::new(r"^\s*(?:public\s+|private\s+|protected\s+)?(?:abstract\s+|final\s+)?(?:class|interface|enum|record)\s+(\w+)").unwrap();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_class.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "class".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        }
    }

    symbols
}

// ── C/C++ 解析器 ────────────────────────────────────────────────────────

fn parse_c_cpp(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_class = Regex::new(r"^\s*(?:class|struct|enum)\s+(\w+)").unwrap();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_class.captures(line) {
            let kind = line.trim_start().split_whitespace().next().unwrap_or("class").to_string();
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind, name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        }
    }

    symbols
}

// ── C# 解析器 ───────────────────────────────────────────────────────────

fn parse_csharp(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_type = Regex::new(r"^\s*(?:public|private|protected|internal|static|abstract|sealed|readonly|partial\s+)*(?:class|interface|struct|enum|record)\s+(\w+)").unwrap();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_type.captures(line) {
            let kind = line.trim_start().split_whitespace()
                .find(|w| ["class", "interface", "struct", "enum", "record"].contains(w))
                .unwrap_or("class").to_string();
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind, name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        }
    }

    symbols
}

// ── Kotlin 解析器 ───────────────────────────────────────────────────────

fn parse_kotlin(file: &str, content: &str, _: &Regex, _: &Regex) -> Vec<CodeSymbol> {
    let mut symbols = Vec::new();
    let lines: Vec<&str> = content.lines().collect();

    let re_fun = Regex::new(r"^\s*(?:public|private|protected|internal|override|suspend|inline|tailrec|operator|infix)\s+*(?:fun\s+)(\w+)").unwrap();
    let re_class = Regex::new(r"^\s*(?:public|private|protected|internal|data|sealed|open|abstract)\s+*(?:class|interface|object|enum)\s+(\w+)").unwrap();

    for (i, line) in lines.iter().enumerate() {
        if let Some(cap) = re_fun.captures(line) {
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: "fun".into(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        } else if let Some(cap) = re_class.captures(line) {
            let kind = line.trim_start().split_whitespace()
                .find(|w| ["class", "interface", "object", "enum"].contains(w))
                .unwrap_or("class");
            symbols.push(CodeSymbol {
                file: file.to_string(), line: i + 1,
                kind: kind.to_string(), name: cap[1].to_string(),
                parent: None, signature: line.trim().to_string(),
            });
        }
    }

    symbols
}

// ── 工具执行 ────────────────────────────────────────────────────────────

#[async_trait::async_trait]
impl Tool for CodeIndex {
    async fn execute(&self, _ctx: &ToolContext, args: &Value) -> Result<ToolResult, String> {
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some("outline") => "outline",
            Some("search") => "search",
            _ => return Err("code_index: 'action' must be 'outline' or 'search'".into()),
        };

        let query = if action == "search" {
            match args.get("query").and_then(|v| v.as_str()) {
                Some(q) if !q.is_empty() => Some(q.to_lowercase()),
                _ => return Err("code_index: 'query' is required for action='search'".into()),
            }
        } else {
            args.get("query").and_then(|v| v.as_str()).map(|q| q.to_lowercase())
        };

        let path_str = args
            .get("path")
            .and_then(|v| v.as_str())
            .filter(|p| !p.is_empty())
            .unwrap_or(".");

        let kind_filter = args.get("kind").and_then(|v| v.as_str()).map(|s| s.to_lowercase());

        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|l| (l as usize).clamp(1, CODE_INDEX_MAX_LIMIT))
            .unwrap_or(CODE_INDEX_DEFAULT_LIMIT);

        let path = self.resolve_path(path_str)?;

        if !path.exists() {
            return Err(format!("code_index: path '{}' does not exist", path_str));
        }

        // 收集符号
        let mut symbols: Vec<CodeSymbol> = Vec::new();
        let mut file_count = 0;

        if path.is_file() {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                if let Some(lang) = Self::lang_for_ext(ext) {
                    symbols = Self::parse_file(&path, lang);
                }
            }
        } else if path.is_dir() {
            let walker = ignore::WalkBuilder::new(path)
                .standard_filters(true)
                .build();

            for entry in walker {
                if file_count >= CODE_INDEX_MAX_FILES {
                    break;
                }
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    continue;
                }
                let ext = match entry.path().extension().and_then(|e| e.to_str()) {
                    Some(e) => e,
                    None => continue,
                };
                let lang = match Self::lang_for_ext(ext) {
                    Some(l) => l,
                    None => continue,
                };
                file_count += 1;
                let file_symbols = Self::parse_file(entry.path(), lang);
                symbols.extend(file_symbols);
            }
        }

        // 过滤
        let query_lower = query.as_ref().map(|q| q.to_lowercase());
        let kind_lower = kind_filter.as_ref().map(|k| k.to_lowercase());

        let filtered: Vec<&CodeSymbol> = symbols.iter()
            .filter(|s| {
                if let Some(ref q) = query_lower {
                    if !s.name.to_lowercase().contains(q.as_str())
                        && !s.signature.to_lowercase().contains(q.as_str())
                    {
                        return false;
                    }
                }
                if let Some(ref k) = kind_lower {
                    if s.kind.to_lowercase() != *k {
                        return false;
                    }
                }
                true
            })
            .take(limit)
            .collect();

        if filtered.is_empty() {
            let msg = match action {
                "search" => format!("code_index: no symbols found matching '{}'", query.unwrap_or_default()),
                _ => "code_index: no symbols found".into(),
            };
            return Ok(ToolResult::ok(msg));
        }

        // 格式化输出
        let mut output = String::new();
        for sym in &filtered {
            let parent_str = sym.parent.as_ref().map(|p| format!("{}.", p)).unwrap_or_default();
            output.push_str(&format!(
                "{}:{}: {} {}{} — {}\n",
                sym.file, sym.line, sym.kind, parent_str, sym.name, sym.signature
            ));
        }

        if filtered.len() >= limit || symbols.len() > limit {
            output.push_str("... (truncated; narrow path/query/kind or raise limit)\n");
        }

        Ok(ToolResult::ok(output.trim_end().to_string()))
    }
}


#[async_trait::async_trait]
impl CheckableTool for CodeIndex {
    fn check(&self, _ctx: &ToolContext, _args: &serde_json::Value) -> Decision {
        Decision::Allow
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::sync::atomic::{AtomicU64, Ordering};
    use llm::tool::ToolMeta;

    static TEST_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir();
        let id = TEST_COUNTER.fetch_add(1, Ordering::Relaxed);
        dir.join(format!("_test_code_index_{}_{}", std::process::id(), id))
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
    async fn outline_rust_file() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test.rs"), b"\
pub fn hello() {}\n\
struct MyStruct {}\n\
enum MyEnum {}\n\
trait MyTrait {}\n\
mod my_mod;\n\
const MAX: usize = 100;\n\
macro_rules! my_macro {}\n").unwrap();

        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "action": "outline", "path": dir.join("test.rs").to_str().unwrap()
        })).await;

        assert!(result.error().is_none(), "error: {:?}", result.error());
        assert!(result.output().contains("hello"));
        assert!(result.output().contains("MyStruct"));
        assert!(result.output().contains("MyEnum"));
        assert!(result.output().contains("MyTrait"));
        assert!(result.output().contains("MAX"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_symbol() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lib.rs"), b"\
pub fn add(a: i32, b: i32) -> i32 { a + b }\n\
pub fn subtract(a: i32, b: i32) -> i32 { a - b }\n\
struct Config {}\n").unwrap();

        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "action": "search", "query": "add", "path": dir.to_str().unwrap()
        })).await;

        assert!(result.error().is_none());
        assert!(result.output().contains("add"), "output: {}", result.output());
        assert!(!result.output().contains("subtract"), "output: {}", result.output());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn filter_by_kind() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test.rs"), b"fn do_something() {}\nstruct Data {}\nenum Status {}\n").unwrap();

        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "action": "outline", "path": dir.to_str().unwrap(), "kind": "struct"
        })).await;

        assert!(result.error().is_none());
        assert!(result.output().contains("Data"));
        assert!(!result.output().contains("do_something"));
        assert!(!result.output().contains("Status"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unsupported_file_type() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("data.json"), b"{\"key\": \"value\"}").unwrap();
        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "action": "outline", "path": dir.to_str().unwrap()
        })).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("no symbols found"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_action() {
        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({})).await;
        assert!(result.error().is_some());
    }

    #[tokio::test]
    async fn python_symbols() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.py"), b"class MyClass:\n    def method(self): pass\ndef top_level(): pass\n").unwrap();
        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "action": "outline", "path": dir.to_str().unwrap()
        })).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("MyClass"));
        assert!(result.output().contains("top_level"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn jsts_symbols() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("app.ts"), b"\
function greet(name: string): string { return ''; }\n\
class App {}\ninterface Config {}\ntype Data = string;\nenum Color { Red }\nconst MAX = 100;\n").unwrap();
        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "action": "outline", "path": dir.to_str().unwrap()
        })).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("greet"));
        assert!(result.output().contains("App"));
        assert!(result.output().contains("Config"));
        assert!(result.output().contains("Data"));
        assert!(result.output().contains("Color"));
        assert!(result.output().contains("MAX"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn go_symbols() {
        let dir = temp_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.go"), b"package main\nfunc hello() {}\ntype Config struct {}\ntype Reader interface {}\n").unwrap();
        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({
            "action": "outline", "path": dir.to_str().unwrap()
        })).await;
        assert!(result.error().is_none());
        assert!(result.output().contains("hello"));
        assert!(result.output().contains("Config"));
        assert!(result.output().contains("Reader"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn search_requires_query() {
        let tool = CodeIndex::new(std::env::temp_dir());
        let result = tool.execute(&test_ctx(), &serde_json::json!({"action": "search"})).await;
        assert!(result.error().is_some());
        assert!(result.error().unwrap().contains("query"));
    }
}
