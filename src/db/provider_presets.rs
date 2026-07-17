// ── 内置 Provider 预设 ──
//
// 参照 Reasonix 的 provider_presets.go，在编译时嵌入常用 LLM 服务商模板。
// 每个预设不含 api_key —— 密钥由用户通过设置页面填写，存入 AppConfig JSON。

/// 单个 provider 预设
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub kind: &'static str,      // "openai" | "anthropic"
    pub base_url: &'static str,
    pub models: &'static [&'static str],
    pub models_url: &'static str,
    pub context_window: u32,
}

/// 返回所有内置 provider 预设列表
pub fn all_presets() -> &'static [ProviderPreset] {
    &PRESETS
}

static PRESETS: &[ProviderPreset] = &[
    // ── DeepSeek ──
    ProviderPreset {
        id: "deepseek", name: "DeepSeek", kind: "openai",
        base_url: "https://api.deepseek.com",
        models: &[],
        models_url: "", context_window: 1_000_000,
    },
    // ── OpenAI ──
    ProviderPreset {
        id: "openai", name: "OpenAI", kind: "openai",
        base_url: "https://api.openai.com/v1",
        models: &[],
        models_url: "", context_window: 128_000,
    },
    // ── Anthropic ──
    ProviderPreset {
        id: "anthropic", name: "Anthropic", kind: "anthropic",
        base_url: "https://api.anthropic.com",
        models: &[],
        models_url: "", context_window: 200_000,
    },
    // ── Kimi CN ──
    ProviderPreset {
        id: "kimi-cn", name: "Kimi CN", kind: "openai",
        base_url: "https://api.moonshot.cn/v1",
        models: &[],
        models_url: "", context_window: 262_144,
    },
    // ── Kimi Global ──
    ProviderPreset {
        id: "kimi-global", name: "Kimi Global", kind: "openai",
        base_url: "https://api.moonshot.ai/v1",
        models: &[],
        models_url: "", context_window: 262_144,
    },
    // ── GLM CN ──
    ProviderPreset {
        id: "glm-cn", name: "GLM CN", kind: "openai",
        base_url: "https://open.bigmodel.cn/api/paas/v4",
        models: &["glm-5.2", "glm-5.1", "glm-5", "glm-4.7", "glm-4.7-flash"],
        models_url: "", context_window: 1_000_000,
    },
    // ── Z.AI Global ──
    ProviderPreset {
        id: "zai-global", name: "Z.AI Global", kind: "openai",
        base_url: "https://api.z.ai/api/paas/v4",
        models: &["glm-5.2", "glm-5.1", "glm-5", "glm-4.7", "glm-4.7-flash"],
        models_url: "", context_window: 1_000_000,
    },
    // ── MiniMax CN ──
    ProviderPreset {
        id: "minimax-cn", name: "MiniMax CN", kind: "openai",
        base_url: "https://api.minimaxi.com/v1",
        models: &["MiniMax-M3", "MiniMax-M2.7", "MiniMax-M2.7-highspeed"],
        models_url: "", context_window: 1_048_576,
    },
    // ── Qwen CN ──
    ProviderPreset {
        id: "qwen-cn", name: "Qwen CN", kind: "openai",
        base_url: "https://dashscope.aliyuncs.com/compatible-mode/v1",
        models: &["qwen3.7-plus", "qwen3.7-max", "qwen3.6-plus", "qwen3-coder-next"],
        models_url: "", context_window: 131_072,
    },
    // ── Qwen Global ──
    ProviderPreset {
        id: "qwen-global", name: "Qwen Global", kind: "openai",
        base_url: "https://dashscope-intl.aliyuncs.com/compatible-mode/v1",
        models: &["qwen3.7-plus", "qwen3.7-max", "qwen3.6-plus", "qwen3-coder-next"],
        models_url: "", context_window: 131_072,
    },
    // ── StepFun ──
    ProviderPreset {
        id: "stepfun", name: "StepFun", kind: "openai",
        base_url: "https://api.stepfun.com/step_plan/v1",
        models: &["step-3.7-flash", "step-3.5-flash"],
        models_url: "", context_window: 131_072,
    },
    // ── NovitaAI ──
    ProviderPreset {
        id: "novita", name: "NovitaAI", kind: "openai",
        base_url: "https://api.novita.ai/openai/v1",
        models: &["deepseek/deepseek-v4-pro", "qwen/qwen3.7-max", "zai-org/glm-5.2"],
        models_url: "", context_window: 131_072,
    },
    // ── HuggingFace ──
    ProviderPreset {
        id: "huggingface", name: "HuggingFace", kind: "openai",
        base_url: "https://router.huggingface.co/v1",
        models: &["zai-org/GLM-5.2", "Qwen/Qwen3.5-72B-Instruct", "deepseek-ai/DeepSeek-V3.2"],
        models_url: "", context_window: 131_072,
    },
    // ── Ollama Cloud ──
    ProviderPreset {
        id: "ollama-cloud", name: "Ollama Cloud", kind: "openai",
        base_url: "https://ollama.com/v1",
        models: &["glm-5.2", "kimi-k2.7-code", "deepseek-v4-pro", "deepseek-v4-flash"],
        models_url: "", context_window: 131_072,
    },
];
