mod ui_launch {
    use racpagent::ui::components::app::App;
    pub fn run() {
        dioxus::launch(App);
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

    ui_launch::run();
}
