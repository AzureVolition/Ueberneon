// explore_tools_example.rs —— 演示 Ls / Glob / CodeIndex / WebFetch / Grep 工具。
//
// 运行方式：cargo run --example explore_tools_example
//
// 不依赖 LLM API，直接调用工具执行，展示所有只读浏览工具的用法。

use llm::tool::{AgentMode, Tool, ToolContext};
use racpagent::tools::{
    CodeIndex, Glob, Grep, Ls, WebFetch,
};

#[tokio::main]
async fn main() {
    println!("═══ 只读浏览工具集演示 ═══\n");

    let ctx = ToolContext {
        call_id: "demo".into(),
        plan_mode: false,
        agent_mode: AgentMode::Ask,
        progress: None,
    };

    // ── 1. Ls：目录列表 ─────────────────────────────────
    println!("─── 1. Ls：列出 src/tools 目录 ───");
    let tool = Ls::new();
    let result = tool.execute(&ctx, &serde_json::json!({
        "path": "src/tools",
    })).await;
    print_result("ls", &result);

    // ── 2. Ls 递归模式 ─────────────────────────────────
    println!("─── 2. Ls：递归列出 src/tools/internal ───");
    let result = tool.execute(&ctx, &serde_json::json!({
        "path": "src/tools/internal",
        "recursive": true,
    })).await;
    print_result("ls -R", &result);

    // ── 3. Glob：文件匹配 ──────────────────────────────
    println!("─── 3. Glob：匹配所有 .rs 文件 ───");
    let tool = Glob::new();
    let result = tool.execute(&ctx, &serde_json::json!({
        "pattern": "src/**/*.rs",
    })).await;
    print_result_count("glob", &result, 20);

    // ── 4. Glob：精确文件名 ────────────────────────────
    println!("─── 4. Glob：匹配 Cargo.toml ───");
    let result = tool.execute(&ctx, &serde_json::json!({
        "pattern": "**/Cargo.toml",
    })).await;
    print_result("glob", &result);

    // ── 5. Grep：代码搜索 ──────────────────────────────
    println!("─── 5. Grep：搜索 \"pub fn\" ───");
    let tool = Grep::new();
    let result = tool.execute(&ctx, &serde_json::json!({
        "pattern": "^pub fn",
        "path": "src",
    })).await;
    print_result_count("grep", &result, 15);

    // ── 6. Grep：项目中的 TODO ─────────────────────────
    println!("─── 6. Grep：搜索 TODO 注释 ───");
    let result = tool.execute(&ctx, &serde_json::json!({
        "pattern": "TODO",
        "path": "src",
    })).await;
    print_result("grep", &result);

    // ── 7. CodeIndex：Rust 符号索引 ────────────────────
    println!("─── 7. CodeIndex：outline src/tools/internal/bash.rs ───");
    let tool = CodeIndex::new();
    let result = tool.execute(&ctx, &serde_json::json!({
        "action": "outline",
        "path": "src/tools/internal/bash.rs",
    })).await;
    print_result("code_index", &result);

    // ── 8. CodeIndex：搜索模式 ─────────────────────────
    println!("─── 8. CodeIndex：搜索名为 \"new\" 的符号 ───");
    let result = tool.execute(&ctx, &serde_json::json!({
        "action": "search",
        "query": "new",
        "path": "src/tools",
        "kind": "fn",
    })).await;
    print_result("code_index search", &result);

    // ── 9. CodeIndex：按 kind 过滤 ─────────────────────
    println!("─── 9. CodeIndex：列出所有 struct ───");
    let result = tool.execute(&ctx, &serde_json::json!({
        "action": "outline",
        "path": "src/tools",
        "kind": "struct",
    })).await;
    print_result("code_index kind=struct", &result);

    // ── 10. CodeIndex：Python 符号 ─────────────────────
    println!("─── 10. CodeIndex：多语言支持演示 ───");
    let result = tool.execute(&ctx, &serde_json::json!({
        "action": "outline",
        "path": "src",
        "kind": "fn",
    })).await;
    print_result_count("code_index fn", &result, 10);

    // ── 11. WebFetch：URL 内容 ─────────────────────────
    println!("─── 11. WebFetch：抓取 example.com ───");
    let tool = WebFetch::new();
    let result = tool.execute(&ctx, &serde_json::json!({
        "url": "https://example.com",
    })).await;
    print_result("web_fetch", &result);

    // ── 12. 综合场景：代码分析 ─────────────────────────
    println!("─── 12. 综合场景：搜索 impl 块 + 列出目录 ───");
    let grep = Grep::new();
    let ls = Ls::new();

    // 先搜索 impl
    let grep_result = grep.execute(&ctx, &serde_json::json!({
        "pattern": "impl .+ for",
        "path": "src/tools",
    })).await;

    // 再列出目录结构
    let ls_result = ls.execute(&ctx, &serde_json::json!({
        "path": "src/tools",
        "recursive": false,
    })).await;

    println!("实现（impl ... for）模式匹配：");
    print_result("grep (impl ... for)", &grep_result);
    println!("工具目录结构：");
    print_result("ls", &ls_result);

    println!("═══ 演示结束 ═══");
}

/// 打印工具执行结果。
fn print_result(label: &str, result: &llm::tool::ToolResult) {
    if let Some(err) = result.error() {
        eprintln!("  [{label}] ❌ 错误: {err}\n");
    } else if result.is_blocked() {
        eprintln!("  [{label}] 🔒 被阻止: {}\n", result.output());
    } else if result.output().contains("no matches") || result.output().contains("no symbols found") {
        println!("  ℹ️  {}\n", result.output().trim());
    } else {
        let preview: Vec<&str> = result.output().lines().take(15).collect();
        for line in &preview {
            println!("  {}", line);
        }
        if preview.len() < result.output().lines().count() {
            println!("  ... ({} lines total)", result.output().lines().count());
        }
        println!();
    }
}

/// 打印工具执行结果，只显示前 N 行并计数。
fn print_result_count(label: &str, result: &llm::tool::ToolResult, show_lines: usize) {
    if let Some(err) = result.error() {
        eprintln!("  [{label}] ❌ 错误: {err}\n");
    } else if result.is_blocked() {
        eprintln!("  [{label}] 🔒 被阻止: {}\n", result.output());
    } else if result.output().contains("no matches") || result.output().contains("no symbols found") {
        println!("  ℹ️  {}\n", result.output().trim());
    } else {
        let lines: Vec<&str> = result.output().lines().collect();
        let total = lines.len();
        for line in lines.iter().take(show_lines) {
            println!("  {}", line);
        }
        if total > show_lines {
            println!("  ... ({} total results)", total);
        } else {
            println!("  ({} results)", total);
        }
        println!();
    }
}
