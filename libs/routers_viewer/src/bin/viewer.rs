#![warn(clippy::all, rust_2018_idioms)]

use eframe::App;
use log::info;
use routers_viewer::Application;

#[tokio::main]
#[cfg(not(target_arch = "wasm32"))]
async fn main() -> eframe::Result<()> {
    env_logger::init();

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_fullscreen(cfg!(not(debug_assertions)))
            .with_maximized(cfg!(not(debug_assertions)))
            .with_min_inner_size([300.0, 220.0]),
        ..Default::default()
    };

    info!("Running application");

    eframe::run_native(
        "routers",
        native_options,
        Box::new(|ctx| match Application::new(ctx) {
            Ok(app) => Ok(Box::new(app) as Box<dyn App>),
            Err(e) => Err(e.into_boxed_dyn_error()),
        }),
    )
}

#[cfg(target_arch = "wasm32")]
fn main() {
    eframe::WebLogger::init(log::LevelFilter::Debug).ok();

    let web_options = eframe::WebOptions::default();

    wasm_bindgen_futures::spawn_local(async {
        eframe::WebRunner::new()
            .start(
                "the_canvas_id",
                web_options,
                Box::new(|cc| Box::new(ui::Application::new(cc))),
            )
            .await
            .expect("failed to start eframe");
    });
}
