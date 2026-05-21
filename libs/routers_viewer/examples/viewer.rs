use routers_viewer::ViewerApp;

use eframe::NativeOptions;
use routers_codec::osm::OsmNetwork;
use routers_fixtures::{SYDNEY, SYDNEY_SAVED, fixture};
use tokio::time::Instant;

#[tokio::main]
async fn main() -> eframe::Result<()> {
    env_logger::init();
    let native_options = NativeOptions::default();

    let pbf_path = fixture!(SYDNEY);
    let saved_path = fixture!(SYDNEY_SAVED);

    println!("Opening or ingesting road network...");

    let now = Instant::now();
    let network =
        OsmNetwork::from_pbf_and_save(pbf_path, saved_path).expect("Network must be created");

    println!("Openened in {:?}", now.elapsed());

    eframe::run_native(
        "Routers Map Matcher",
        native_options,
        Box::new(|cc| {
            let ctx = cc.egui_ctx.clone();

            Ok(Box::new(ViewerApp::new(ctx, network)))
        }),
    )
}
