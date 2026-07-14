// registry —— 工具注册表。
//
// 管理工具的生命周期：注册、查找、批量删除、Schema 缓存。

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};

use crate::tools::internal::common::checkable_tool::CheckableTool;
use crate::agent::Tool;
use llm::ToolSchema;


// ── Registry ─────────────────────────────────────────────────────────────────

/// 运行时工具注册表。
///
/// 关键设计：
/// - `add` 时立即缓存规范化 Schema（每个工具的 schema 只序列化一次）
/// - `schemas()` 按名字母序返回（同样的工具集 → 相同的 JSON bytes → 前缀缓存命中）
/// - `remove_prefix` 批量删除 MCP 工具
pub struct Registry {
    tools: RwLock<ToolsInner>,
}

struct ToolsInner {
    map: BTreeMap<String, Arc<dyn CheckableTool + Send + Sync>>,
    order: Vec<String>,
    canon: BTreeMap<String, serde_json::Value>,
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
    pub fn add(&self, tool: Box<dyn CheckableTool>) {
        let mut inner = self.tools.write().unwrap();
        let name = tool.name().to_string();

        if !inner.map.contains_key(&name) {
            inner.order.push(name.clone());
        }

        let mut schema = tool.schema();
        canonicalize_schema(&mut schema);
        inner.canon.insert(name.clone(), schema);
        let boxed: Box<dyn CheckableTool + Send + Sync> = tool;
        inner.map.insert(name, Arc::from(boxed));
    }

    /// 批量删除以 prefix 开头的工具。
    /// 用于 MCP 服务器断开时清理 `mcp__<server>__*`。
    /// 返回删除数量。
    pub fn remove_prefix(&self, prefix: &str) -> usize {
        let mut inner = self.tools.write().unwrap();

        let to_remove: Vec<String> = inner.order.iter()
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect();

        let removed = to_remove.len();

        inner.order.retain(|name| !name.starts_with(prefix));

        for name in &to_remove {
            inner.map.remove(name);
            inner.canon.remove(name);
        }

        removed
    }

    /// 按名查找工具。
    pub fn get(&self, name: &str) -> Option<Arc<dyn CheckableTool + Send + Sync>> {
        self.tools.read().unwrap().map.get(name).cloned()
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
pub fn canonicalize_schema(schema: &mut serde_json::Value) {
    let obj = match schema.as_object_mut() {
        Some(o) => o,
        None => return,
    };

    if let Some(req) = obj.get("required") {
        if req.is_boolean() {
            obj.remove("required");
        }
    }

    for key in &["$defs", "definitions"] {
        if let Some(serde_json::Value::Object(defs)) = obj.get_mut(*key) {
            for def in defs.values_mut() {
                canonicalize_schema(def);
            }
        }
    }

    if let Some(serde_json::Value::Object(props)) = obj.get_mut("properties") {
        let sorted: BTreeMap<String, serde_json::Value> =
            std::mem::take(props).into_iter().collect();
        *props = sorted.into_iter().collect();
    }

    if let Some(serde_json::Value::Array(req)) = obj.get_mut("required") {
        req.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
    }
}

// ── MCP 工具名解析 ───────────────────────────────────────────────────────────

/// 将 `mcp__<server>__<tool>` 拆解为 (server, tool)。
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
pub async fn preview_change(tool: &dyn Tool, _args: &serde_json::Value) -> Option<FileChange> {
    if tool.read_only() {
        return None;
    }
    None
}
