// ── ToolMeta trait ──────────────────────────────────────────────────────────

/// 模型可调用的工具
pub trait ToolMeta: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema，定义工具参数
    fn schema(&self) -> serde_json::Value {
        serde_json::from_str(self.schema_str_str()).unwrap_or_default()
    }
    /// 是否无副作用。Agent 据此决定并行/串行执行。
    fn read_only(&self) -> bool;

    fn schema_str_str(&self) -> &str;
}
