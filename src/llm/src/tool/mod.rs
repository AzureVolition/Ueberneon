
// ── Tool trait ──────────────────────────────────────────────────────────────

/// 模型可调用的工具 
pub trait ToolMeta: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    /// JSON Schema，定义工具参数
    fn schema(&self) -> serde_json::Value;
    /// 是否无副作用。Agent 据此决定并行/串行执行。
    fn read_only(&self) -> bool;
}

#[async_trait::async_trait]
pub trait Tool: ToolMeta {
    /// 执行工具，接收模型生成的 raw JSON args，返回文本结果
    async fn execute(&self, ctx: &ToolContext, args: &serde_json::Value) -> ToolResult;
}

// ── ToolContext ──────────────────────────────────────────────────────────────

/// 工具执行上下文 
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
