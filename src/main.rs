mod ui_launch {
    use ueberneon::ui::components::app::App;

    /// 自定义菜单:去掉原生 Copy 项,让 Cmd+C 落到 WebView,
    /// 由阅读器的自绘选区处理(原生 Copy 依赖系统选区,已被禁用)。
    fn app_menu() -> dioxus::desktop::muda::Menu {
        use dioxus::desktop::muda::{Menu, PredefinedMenuItem, Submenu};

        let menu = Menu::new();

        let app_menu = Submenu::new("UeberNeon", true);
        app_menu
            .append_items(&[
                &PredefinedMenuItem::about(Some("UeberNeon"), None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::hide(None),
                &PredefinedMenuItem::hide_others(None),
                &PredefinedMenuItem::show_all(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::quit(None),
            ])
            .unwrap();

        let edit_menu = Submenu::new("Edit", true);
        edit_menu
            .append_items(&[
                &PredefinedMenuItem::undo(None),
                &PredefinedMenuItem::redo(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::cut(None),
                &PredefinedMenuItem::paste(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::select_all(None),
            ])
            .unwrap();

        let window_menu = Submenu::new("Window", true);
        window_menu
            .append_items(&[
                &PredefinedMenuItem::fullscreen(None),
                &PredefinedMenuItem::separator(),
                &PredefinedMenuItem::minimize(None),
                &PredefinedMenuItem::maximize(None),
                &PredefinedMenuItem::close_window(None),
            ])
            .unwrap();

        menu.append_items(&[&app_menu, &edit_menu, &window_menu])
            .unwrap();
        menu
    }

    pub fn run() {
        dioxus::LaunchBuilder::new()
            .with_cfg(
                dioxus::desktop::Config::new()
                    .with_menu(app_menu())
                    .with_window(
                        dioxus::desktop::WindowBuilder::new()
                            .with_title("UeberNeon")
                            .with_focused(true),
                    ),
            )
            .launch(App);
    }
}

fn main() {
    dotenvy::dotenv().ok();

    // 初始化 tracing 日志（输出到 stderr），含文件:行号
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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
    for meta in inventory::iter::<ueberneon::tools::InternalToolMeta> {
        let _ = writeln!(
            stderr,
            "  {:15} | ro={:5} | {:10} | {}",
            meta.name,
            meta.read_only,
            meta.schema.as_str(),
            meta.description,
        );
    }
    let total = inventory::iter::<ueberneon::tools::InternalToolMeta>
        .into_iter()
        .count();
    let _ = writeln!(stderr, "Total: {} tools", total);
    let _ = stderr.flush();
}
