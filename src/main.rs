mod ui_launch {
    use racpagent::ui::components::app::App;
    pub fn run() {
        dioxus::LaunchBuilder::new()
            .with_cfg(dioxus::desktop::Config::new()
                .with_window(dioxus::desktop::WindowBuilder::new()
                    .with_title("RacpAgent")
                    .with_focused(true)))
            .launch(App);
    }
}

fn main() {
    dotenvy::dotenv().ok();

    // 初始化 tracing 日志（输出到 stderr），含文件:行号
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .with_writer(std::io::stderr)
        .with_file(true)
        .with_line_number(true)
        .init();

    _print_tools_inventory();

    ui_launch::run();
}

/// 启动时打印内部工具清单（验证 inventory 收集效果）。
/// 仅在非测试编译时有效。
fn _print_tools_inventory() {
    use std::io::Write;
    let mut stderr = std::io::stderr();
    let _ = writeln!(stderr, "=== Internal Tools Inventory ===");
    for meta in inventory::iter::<racpagent::tools::InternalToolMeta> {
        let _ = writeln!(stderr, "  {:15} | ro={:5} | {:10} | {}",
            meta.name, meta.read_only,
            meta.schema,
            meta.description,
        );
    }
    let total = inventory::iter::<racpagent::tools::InternalToolMeta>.into_iter().count();
    let _ = writeln!(stderr, "Total: {} tools", total);
    let _ = stderr.flush();
}
