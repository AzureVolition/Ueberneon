
use std::collections::BTreeMap;
use std::sync::RwLock;
use serde::{Deserialize, Serialize};

// ── Tool trait ──────────────────────────────────────────────────────────────

/// 模型可调用的工具。5 个方法对齐 Reasonix 的 tool.Tool 接口。
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema，定义工具参数
    fn schema(&self) -> serde_json::Value;
    /// 执行工具，接收模型生成的 raw JSON args，返回文本结果
    async fn execute(&self, ctx: &ToolContext, args: &serde_json::Value) -> ToolResult;
    /// 是否无副作用。Agent 据此决定并行/串行执行。
    fn read_only(&self) -> bool;
}

// ── ToolContext ──────────────────────────────────────────────────────────────

/// 工具执行上下文，对齐 Reasonix 中通过 context.WithValue 传递的 callID / sink 等。
pub struct ToolContext {
    /// 工具调用的唯一 ID（stream 中 LLM 返回的 tool_call_id）
    pub call_id: String,
    /// 是否在 plan mode（写工具被阻止）
    pub plan_mode: bool,
    /// 流式输出回调，长运行工具推送实时输出到前端
    pub progress: Option<Box<dyn Fn(&str) + Send + Sync>>,
}

// ── ToolResult ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ToolResult {
    /// 返回给模型的文本
    pub output: String,
    /// 错误信息（不为空时 output 仍有效，模型同时看到两者）
    pub error: Option<String>,
    /// 是否被门禁阻止（plan mode / permission gate）
    pub blocked: bool,
    /// 输出是否被截断（> 32KB）
    pub truncated: bool,
}

impl ToolResult {
    pub fn ok(output: impl Into<String>) -> Self {
        Self {
            output: output.into(),
            error: None,
            blocked: false,
            truncated: false,
        }
    }

    pub fn err(error: impl Into<String>) -> Self {
        Self {
            output: String::new(),
            error: Some(error.into()),
            blocked: false,
            truncated: false,
        }
    }

    pub fn blocked(reason: impl Into<String>) -> Self {
        Self {
            output: reason.into(),
            error: None,
            blocked: true,
            truncated: false,
        }
    }
}

// ── ToolSchema ───────────────────────────────────────────────────────────────

/// 传给 LLM 的工具定义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

// ── 全局内置工具池 ──────────────────────────────────────────────────────────

use std::sync::OnceLock;

static BUILTINS: OnceLock<RwLock<BTreeMap<String, Box<dyn Tool>>>> = OnceLock::new();

fn builtins() -> &'static RwLock<BTreeMap<String, Box<dyn Tool>>> {
    BUILTINS.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// 注册编译期内置工具。各工具 crate 在启动时调用。
pub fn register_builtin(tool: Box<dyn Tool>) {
    let name = tool.name().to_string();
    let mut map = builtins().write().unwrap();
    if map.contains_key(&name) {
        panic!("tool: duplicate built-in {name}");
    }
    map.insert(name, tool);
}

/// 返回所有已注册的内置工具（按名称排序）。
pub fn builtins_all() -> Vec<&'static dyn Tool> {
    // 需要特殊处理：BTreeMap 里是 Box<dyn Tool>，返回引用需要 unsafe 或改架构
    // 实际使用中建议直接返回 Vec<(String, Box<dyn Tool>)> 由调用方 clone
    unimplemented!("use builtins_clone() for owned access")
}

/// 返回所有内置工具的克隆（workspace 绑定需要）
pub fn builtins_clone_names() -> Vec<String> {
    builtins().read().unwrap().keys().cloned().collect()
}

pub fn lookup_builtin(name: &str) -> Option<Box<dyn Tool>> {
    // 内置工具一般是无状态的，可以 clone
    // 实际实现中需要一个 Clone 约束或工厂方法
    builtins().read().unwrap().get(name).map(|_| unimplemented!("clone tool"))
}

// ── Registry ─────────────────────────────────────────────────────────────────

/// 运行时工具注册表。对齐 Reasonix tool.Registry。
///
/// 关键设计：
/// - `Add` 时立即缓存规范化 Schema（每个工具的 schema 只序列化一次）
/// - `schemas()` 按名字母序返回（同样的工具集 → 相同的 JSON bytes → 前缀缓存命中）
/// - `remove_prefix` 批量删除 MCP 工具
pub struct Registry {
    tools: RwLock<ToolsInner>,
}

struct ToolsInner {
    map: BTreeMap<String, Box<dyn Tool>>,
    order: Vec<String>,                      // 插入顺序
    canon: BTreeMap<String, serde_json::Value>, // 缓存规范化后的 schema
}

impl Registry {
    pub fn new() -> Self {
        Self {
            tools: RwLock::new(ToolsInner {
                map: BTreeMap::new(),
                order: Vec::new(),
                canon: BTreeMap::new(),
            }),
        }
    }

    /// 插入（或替换）一个工具。保持首次插入顺序。
    /// Schema 在此处规范化并缓存，后续 schemas() 直接返回缓存。
    pub fn add(&self, tool: Box<dyn Tool>) {
        let mut inner = self.tools.write().unwrap();
        let name = tool.name().to_string();

        if !inner.map.contains_key(&name) {
            inner.order.push(name.clone());
        }

        let mut schema = tool.schema();
        canonicalize_schema(&mut schema);
        inner.canon.insert(name.clone(), schema);
        inner.map.insert(name, tool);
    }

    /// 批量删除以 prefix 开头的工具。
    /// 用于 MCP 服务器断开时清理 `mcp__<server>__*`。
    /// 返回删除数量。
    pub fn remove_prefix(&self, prefix: &str) -> usize {
        let mut inner = self.tools.write().unwrap();
        let mut removed = 0;

        inner.order.retain(|name| {
            if name.starts_with(prefix) {
                inner.map.remove(name);
                inner.canon.remove(name);
                removed += 1;
                false
            } else {
                true
            }
        });

        removed
    }

    /// 按名查找工具。
    pub fn get(&self, name: &str) -> Option<Box<dyn Tool>> {
        // 返回 clone 或 Arc<dyn Tool> —— 实际建议用 Arc
        self.tools.read().unwrap().map.get(name).map(|_| unimplemented!("use Arc<dyn Tool>"))
    }

    /// 已注册工具数量。
    pub fn len(&self) -> usize {
        self.tools.read().unwrap().order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 返回工具名列表（插入顺序）。
    pub fn names(&self) -> Vec<String> {
        self.tools.read().unwrap().order.clone()
    }

    /// 返回工具 Schema 列表（按名字母序排序）。
    /// 排序保证相同的工具集产生完全相同的字节序列 → LLM prefix cache 命中。
    pub fn schemas(&self) -> Vec<ToolSchema> {
        let inner = self.tools.read().unwrap();

        let mut names: Vec<&String> = inner.order.iter().collect();
        names.sort();

        names
            .iter()
            .filter_map(|name| {
                let tool = inner.map.get(*name)?;
                let canon = inner.canon.get(*name)?;
                Some(ToolSchema {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters: canon.clone(),
                })
            })
            .collect()
    }
}

// ── Schema 规范化 ────────────────────────────────────────────────────────────

/// 递归规范化 JSON Schema：排序 properties/required 数组、清理 MCP 无效字段。
/// 目的：同构 schema 产生完全相同字节 → 前缀缓存命中。
fn canonicalize_schema(schema: &mut serde_json::Value) {
    let obj = match schema.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    // 清理 MCP 服务器的 "required": true（OpenAPI 风格无效字段）
    if let Some(req) = obj.get("required") {
        if req.is_boolean() {
            obj.remove("required");
        }
    }

    // 递归处理 $defs / definitions
    for key in &["$defs", "definitions"] {
        if let Some(serde_json::Value::Object(defs)) = obj.get_mut(*key) {
            for def in defs.values_mut() {
                canonicalize_schema(def);
            }
        }
    }

    // 排序 properties keys
    if let Some(serde_json::Value::Object(props)) = obj.get_mut("properties") {
        let sorted: BTreeMap<String, serde_json::Value> =
            std::mem::take(props).into_iter().collect();
        *props = sorted.into_iter().collect();
    }

    // 排序 required 数组
    if let Some(serde_json::Value::Array(req)) = obj.get_mut("required") {
        req.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
}

// ── MCP 工具名解析 ───────────────────────────────────────────────────────────

/// 将 `mcp__<server>__<tool>` 拆解为 (server, tool)。
/// 非 MCP 工具返回 None。
pub fn split_mcp_name(name: &str) -> Option<(&str, &str)> {
    let rest = name.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((server, tool))
}

// ── 预览 ─────────────────────────────────────────────────────────────────────

/// 写工具可选实现：预览文件变更而不真正写入。
/// 对齐 Reasonix 的 Previewer 接口。
#[async_trait::async_trait]
pub trait Previewer: Tool {
    async fn preview(&self, args: &serde_json::Value) -> Result<FileChange, PreviewError>;
}

#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: String,
    pub added: usize,
    pub removed: usize,
    pub binary: bool,
}

#[derive(Debug)]
pub struct PreviewError(String);

impl std::fmt::Display for PreviewError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for PreviewError {}

/// 尝试获取预览。只读工具返回 None；未实现 Previewer 或预览失败也返回 None。
pub async fn preview_change(tool: &dyn Tool, args: &serde_json::Value) -> Option<FileChange> {
    if tool.read_only() {
        return None;
    }
    // 需要 downcast: &dyn Tool → &dyn Previewer
    // 实际实现中用 as_any() 或 mopa crate
    None
}