mod ui_launch {
    use racpagent::ui::components::app::App;
    pub fn run() {
        dioxus::launch(App);
    }
}

fn main() {
    dotenvy::dotenv().ok();

    ui_launch::run();
}
