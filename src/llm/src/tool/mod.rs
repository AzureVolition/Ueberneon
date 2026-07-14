
// ── ToolMeta trait ──────────────────────────────────────────────────────────

/// 模型可调用的工具 
pub trait ToolMeta: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema，定义工具参数
    fn schema(&self) -> serde_json::Value;
    /// 是否无副作用。Agent 据此决定并行/串行执行。
    fn read_only(&self) -> bool;
}

