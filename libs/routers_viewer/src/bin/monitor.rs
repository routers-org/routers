#![warn(clippy::all, rust_2018_idioms)]

use eframe::App;
use routers_viewer::monitor::MonitorApp;

#[tokio::main]
async fn main() -> eframe::Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();

    eframe::run_native(
        "routers monitor",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_maximized(true)
                .with_min_inner_size([600.0, 400.0]),
            ..Default::default()
        },
        Box::new(|ctx| match MonitorApp::new(ctx) {
            Ok(app) => Ok(Box::new(app) as Box<dyn App>),
            Err(e) => Err(e.into_boxed_dyn_error()),
        }),
    )
}
