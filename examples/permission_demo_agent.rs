// permission_demo_agent.rs —— 演示权限门禁阻止工具调用的 Agent 循环。
//
// 与 agent_loop_with_tools.rs 相同的 LLM 交互循环，但注册了真实的
// 权限检查（DenySystemPaths、ForcePushGuard、DangerousPatternDetector），
// 并在 Tool Result 中区分 Success / Blocked / Error 三种状态。
//
// 运行方式：cargo run --example permission_demo_agent
// 需要 .env 文件中配置 OPENAI_API_KEY / OPENAI_BASE_URL。

use std::sync::Arc;
use std::time::Duration;

use llm::{
    Chunk, Message, OpenAiProvider, Provider, Request, Role, ToolCall,
};
use racpagent::agent::{AgentMode, ToolContext};
use futures::StreamExt;
use racpagent::permission::checks::{
    DangerousPatternDetector, DenySystemPaths, ForcePushGuard, ReadOnlyBashClassifier,
};
use racpagent::tools::{
    Bash, BashOutput, EditFile, Grep, JobManager, KillShell, Ls,
    MultiEdit, Registry, SandboxSpec, SnapshotStore, WriteFile,
};
use racpagent::tools::content_tracker::FileObserveTracker;
use racpagent::tools::internal::read_file::ReadFile;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 从 .env 或环境变量加载 API 配置
    dotenvy::dotenv().ok();
    let base_url = std::env::var("OPENAI_BASE_URL")
        .unwrap_or_else(|_| "https://api.deepseek.com".into());
    let api_key = std::env::var("OPENAI_API_KEY")
        .or_else(|_| std::env::var("DEEPSEEK_API_KEY"))
        .expect("请在 .env 中设置 OPENAI_API_KEY 或 DEEPSEEK_API_KEY");
    let model = std::env::var("OPENAI_MODEL")
        .unwrap_or_else(|_| "deepseek-v4-flash".into());

    let provider = OpenAiProvider::new(
        "deepseek".into(),
        base_url,
        model,
        api_key,
        Some("high".into()),
        false,
        None,
    )?;

    let registry = Registry::new();

    // ── 共享状态 ──
    let tracker = Arc::new(FileObserveTracker::new());
    let job_manager = Arc::new(JobManager::new());
    let snapshot = Arc::new(SnapshotStore::new());
    let work_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let sandbox = SandboxSpec::defaults(&work_dir);

    // ── 只读工具 ──
    registry.add(Box::new(ReadFile::new(tracker.clone())));
    registry.add(Box::new(Grep::new()));

    // ── 文件变异工具（带 DenySystemPaths 检查）──
    let file_checks = || -> Vec<Box<dyn racpagent::permission::Check>> {
        vec![Box::new(DenySystemPaths)]
    };

    registry.add(Box::new(WriteFile::new(
        work_dir.clone(), snapshot.clone(), file_checks(), tracker.clone(),
    )));
    registry.add(Box::new(EditFile::new(
        work_dir.clone(), snapshot.clone(), file_checks(), tracker.clone(),
    )));
    registry.add(Box::new(MultiEdit::new(
        work_dir.clone(), snapshot.clone(), file_checks(), tracker.clone(),
    )));

    // ── bash 工具（带 ForcePushGuard + DangerousPatternDetector + ReadOnlyBashClassifier）──
    let bash_checks = || -> Vec<Box<dyn racpagent::permission::Check>> {
        vec![
            Box::new(ForcePushGuard),
            Box::new(DangerousPatternDetector),
            Box::new(ReadOnlyBashClassifier),
        ]
    };

    registry.add(Box::new(Bash::new(
        work_dir.clone(),
        Duration::from_secs(120),
        job_manager.clone(),
        Some(sandbox),
        bash_checks(),
    )));
    registry.add(Box::new(BashOutput::new(job_manager.clone())));
    registry.add(Box::new(KillShell::new(job_manager)));

    // ── 请求：提示模型执行一些会被权限系统阻止的操作 ──
    let mut req = Request {
        messages: vec![
            Message {
                role: Role::System,
                content: Some(
                    "你是一个权限演示 Agent。你有一组工具可以使用，其中一些操作会被权限策略阻止。\n\
                     当工具调用被阻止时，结果中会包含 blocked 信息，请根据提示调整你的操作。\n\n\
                     可用的权限检查规则：\n\
                     - DenySystemPaths：禁止编辑 /etc/、/usr/ 等系统路径\n\
                     - ForcePushGuard：git push --force 需要确认\n\
                     - DangerousPatternDetector：rm -rf、sudo 等危险命令需要确认\n\
                     - ReadOnlyBashClassifier：ls、echo、cat 等只读命令自动放行\n\n\
                     请按以下步骤演示：\n\
                     1. 先执行一个安全的只读命令（如 ls 或 echo）\n\
                     2. 尝试写入系统路径 /etc/test.txt（应被阻止）\n\
                     3. 执行一个安全的写操作（在项目目录下写文件）"
                        .into(),
                ),
                ..Default::default()
            },
        ],
        tools: registry.schemas(),
        temperature: 0.3,
        max_tokens: 4096,
    };

    // ── Agent 循环 ──
    let max_rounds = 10;
    for round in 0..max_rounds {
        eprintln!("\n═══ Round {} ═══", round + 1);

        let mut have_tool_calls = false;
        let mut stream = provider.stream(&req).await?;

        let mut output = String::new();
        let mut reasoning_content = String::new();
        let mut pending_tool_calls: Vec<ToolCall> = Vec::new();

        while let Some(result) = stream.next().await {
            match &result {
                Ok(Chunk::Text(t)) => {
                    output.push_str(t);
                    print!("{t}");
                }
                Ok(Chunk::Reasoning { text, .. }) => {
                    reasoning_content.push_str(text);
                }
                Ok(Chunk::Usage(u)) => {
                    eprintln!(
                        "\n[tokens: {} prompt | {} completion]",
                        u.prompt_tokens, u.completion_tokens
                    );
                }
                Ok(Chunk::ToolCallComplete(tool)) => {
                    eprintln!("\n[tool call: {} — {}]", tool.name, tool.arguments);
                    have_tool_calls = true;
                    pending_tool_calls.push(tool.clone());
                }
                Err(e) => eprintln!("\n[stream error: {e}]"),
                _ => {}
            }
        }

        // Push assistant 消息（带上 tool_calls）
        {
            let mut msg = Message {
                role: Role::Assistant,
                content: Some(output),
                reasoning_content: Some(reasoning_content),
                ..Default::default()
            };
            if !pending_tool_calls.is_empty() {
                msg.tool_calls = pending_tool_calls.clone();
            }
            req.messages.push(msg);
        }

        // 执行工具，结果作为 Tool 角色消息推入
        for tool in &pending_tool_calls {
            if let Some(t) = registry.get(&tool.name) {
                let ctx = ToolContext {
                    call_id: tool.id.clone(),
                    plan_mode: false,
                    agent_mode: AgentMode::Ask,
                    progress: None,
                };
                let args: serde_json::Value =
                    serde_json::from_str(&tool.arguments).unwrap_or_default();
                let result = t.checked_execute(&ctx, &args).await;

                // 根据三种结果变体构造不同的反馈消息
                let content = match &result {
                    Ok(tr) => {
                        println!("✅ tool success: {} (truncated={})", &tr.output[..tr.output.len().min(80)], tr.truncated);
                        if tr.truncated {
                            format!("[output truncated]\n{}", tr.output)
                        } else {
                            tr.output.clone()
                        }
                    }
                    Err(msg) => {
                        println!("❌ tool blocked/error: {}", msg);
                        format!("[blocked/error] {}", msg)
                    }
                };

                req.messages.push(Message {
                    role: Role::Tool,
                    content: Some(content),
                    tool_call_id: Some(tool.id.clone()),
                    name: Some(tool.name.clone()),
                    ..Default::default()
                });

            } else {
                eprintln!("\n[error: tool '{}' not found in registry]", tool.name);
            }
        }

        if !have_tool_calls {
            eprintln!("\n[agent finished — no more tool calls]");
            break;
        }
    }

    Ok(())
}
