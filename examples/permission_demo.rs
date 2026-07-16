// permission_demo.rs —— 权限检查系统演示。
//
// 注册带有权限检查的工具，演示 DenySystemPaths、ForcePushGuard、
// DangerousPatternDetector、ReadOnlyBashClassifier 等检查器如何阻止
// 危险操作，以及非交互模式下 Ask 如何退化为 Allow。
//
// 运行方式：cargo run --example permission_demo
//
// 不依赖 LLM API，直接调用工具执行。

use std::sync::Arc;
use std::time::Duration;

use racpagent::agent::{AgentMode, ActionMode, ToolContext};
use racpagent::permission::Check;
use racpagent::permission::checks::*;
use racpagent::tools::content_tracker::FileObserveTracker;
use racpagent::tools::{
    Bash, EditFile, Grep, JobManager, MultiEdit, Registry,
    SandboxSpec, SnapshotStore, WriteFile,
};

#[tokio::main]
async fn main() {
    println!("═══ 权限检查系统演示 ═══\n");

    // ── 注册带权限检查的工具 ──
    let registry = Registry::new();
    let work_dir = std::env::current_dir().unwrap_or_else(|_| ".".into());
    let snapshot = Arc::new(SnapshotStore::new());
    let tracker = Arc::new(FileObserveTracker::new());

    // 文件编辑工具的权限检查（闭包：每次调用创建新 Vec，避免 Clone 约束）
    let file_checks = || -> Vec<Box<dyn Check>> {
        vec![Box::new(DenySystemPaths) as Box<dyn Check>]
    };

    // bash 工具的权限检查
    let bash_checks = || -> Vec<Box<dyn Check>> {
        vec![
            Box::new(ReadOnlyBashClassifier) as Box<dyn Check>,
            Box::new(DangerousPatternDetector) as Box<dyn Check>,
            Box::new(ForcePushGuard) as Box<dyn Check>,
        ]
    };

    registry.add(Box::new(Bash::new(
        work_dir.clone(),
        Duration::from_secs(30),
        Arc::new(JobManager::new()),
        Some(SandboxSpec::defaults(&work_dir)),
        bash_checks(),
    )));

    registry.add(Box::new(EditFile::new(
        work_dir.clone(), snapshot.clone(), file_checks(), tracker.clone(),
    )));
    registry.add(Box::new(MultiEdit::new(
        work_dir.clone(), snapshot.clone(), file_checks(), tracker.clone(),
    )));
    registry.add(Box::new(WriteFile::new(
        work_dir.clone(), snapshot, file_checks(), tracker,
    )));
    registry.add(Box::new(Grep::new(work_dir.clone())));

    let ctx = ToolContext {
        call_id: "demo".into(),
        plan_mode: ActionMode::Regular,
        agent_mode: AgentMode::Ask,
        progress: None,
    };

    // 预创建不同模式下的上下文（避免后续 struct update 移动问题）
    // ToolContext 包含 Box<dyn Fn> 不实现 Clone，需要手动构造
    let ctx_unrestrained = ToolContext {
        agent_mode: AgentMode::Unrestrained,
        call_id: "demo-unrestrained".into(),
        plan_mode: ActionMode::Regular,
        progress: None,
    };
    let ctx_cautious = ToolContext {
        agent_mode: AgentMode::Cautious,
        call_id: "demo-cautious".into(),
        plan_mode: ActionMode::Regular,
        progress: None,
    };

    // ── 场景 1：只读 bash 命令（应被 Allow） ──
    println!("─── 场景 1：只读 bash 命令 ───");
    run_tool("Bash", &registry, &ctx, serde_json::json!({
        "command": "echo 'hello world'"
    })).await;

    // ── 场景 2：危险 bash 命令（应被 DangerousPatternDetector → Ask） ──
    println!("\n─── 场景 2：危险 bash 命令（rm -rf）───");
    run_tool("Bash", &registry, &ctx, serde_json::json!({
        "command": "rm -rf /tmp/foo"
    })).await;

    // ── 场景 3：git force push（应被 ForcePushGuard → Ask） ──
    println!("\n─── 场景 3：git force push ───");
    run_tool("Bash", &registry, &ctx, serde_json::json!({
        "command": "git push --force origin main"
    })).await;

    // ── 场景 4：写入系统路径（应被 DenySystemPaths → Deny） ──
    println!("\n─── 场景 4：写入系统路径 /etc/passwd ───");
    run_tool("WriteFile", &registry, &ctx, serde_json::json!({
        "file_path": "/etc/passwd",
        "content": "hacked",
        "overwrite": true
    })).await;

    // ── 场景 5：sudo 命令（应被 DangerousPatternDetector → Ask） ──
    println!("\n─── 场景 5：sudo 命令 ───");
    run_tool("Bash", &registry, &ctx, serde_json::json!({
        "command": "sudo rm -rf /var/log"
    })).await;

    // ── 场景 6：Unrestrained 模式下 force push（Ask 降级为 Allow） ──
    println!("\n─── 场景 6：Unrestrained 模式 + git push --force ───");
    run_tool("Bash", &registry, &ctx_unrestrained, serde_json::json!({
        "command": "git push --force origin main"
    })).await;

    // ── 场景 7：复合命令——只读 + 危险 ──
    println!("\n─── 场景 7：复合命令（ls && rm -rf /tmp）───");
    run_tool("Bash", &registry, &ctx, serde_json::json!({
        "command": "ls -la && rm -rf /tmp/foo"
    })).await;

    // ── 场景 8：编辑 /etc/passwd（应被 DenySystemPaths → Deny） ──
    println!("\n─── 场景 8：编辑系统路径 /etc/passwd ───");
    run_tool("EditFile", &registry, &ctx, serde_json::json!({
        "file_path": "/etc/passwd",
        "old_string": "root",
        "new_string": "toor"
    })).await;

    // ── 场景 9：Cautious 模式 + 写文件到未知路径（应被提升为 Ask） ──
    println!("\n─── 场景 9：Cautious 模式 + 写入未知路径 ───");
    run_tool("WriteFile", &registry, &ctx_cautious, serde_json::json!({
        "file_path": "/home/user/test.txt",
        "content": "test"
    })).await;

    println!("\n═══ 演示结束 ═══");
}

/// 执行工具并打印结果（含是否被阻止的标记）。
async fn run_tool(tool_name: &str, registry: &Registry, ctx: &ToolContext, args: serde_json::Value) {
    if let Some(tool) = registry.get(tool_name) {
        let result = tool.checked_execute(ctx, &args).await;

        match &result {
            Ok(tr) => {
                let preview: &str = tr.output.lines().next().unwrap_or("");
                println!("  ✅ Success: {}", preview);
                if tr.truncated {
                    println!("     (output truncated)");
                }
            }
            Err(msg) => {
                println!("  ❌ {}", msg);
            }
        }
    } else {
        println!("  ⚠️  Tool '{}' not found in registry", tool_name);
    }
}
