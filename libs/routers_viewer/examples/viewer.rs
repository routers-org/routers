use routers_viewer::ViewerApp;

use eframe::NativeOptions;
use routers_codec::osm::OsmNetwork;
use routers_fixtures::{LOS_ANGELES, LOS_ANGELES_SAVED, fixture};
use tokio::time::Instant;

#[tokio::main]
async fn main() -> eframe::Result<()> {
    env_logger::init();
    let native_options = NativeOptions::default();

    let pbf_path = fixture!(LOS_ANGELES);
    let saved_path = fixture!(LOS_ANGELES_SAVED);

    if !saved_path.exists() {
        let graph = OsmNetwork::from_pbf(pbf_path).expect("Graph must be created");
        graph.save_to_file(saved_path).expect("must save to file");
    }

    println!("File ready! Opening...");
    let now = Instant::now();

    let network =
        OsmNetwork::from_saved(fixture!(LOS_ANGELES_SAVED)).expect("Graph must be created");

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
