use routers_viewer::ViewerApp;

use eframe::NativeOptions;
use routers_codec::osm::OsmNetwork;
use routers_fixtures::{SYDNEY, SYDNEY_SAVED, fixture};

#[tokio::main]
async fn main() -> eframe::Result<()> {
    env_logger::init();
    let native_options = NativeOptions::default();

    let pbf_path = fixture!(SYDNEY);
    let saved_path = fixture!(SYDNEY_SAVED);

    if !saved_path.exists() {
        let graph = OsmNetwork::from_pbf(pbf_path).expect("Graph must be created");
        graph.save_to_file(saved_path).expect("must save to file");
    }

    let network = OsmNetwork::from_saved(fixture!(SYDNEY_SAVED)).expect("Graph must be created");

    eframe::run_native(
        "Routers Map Matcher",
        native_options,
        Box::new(|cc| {
            let ctx = cc.egui_ctx.clone();

            Ok(Box::new(ViewerApp::new(ctx, network)))
        }),
    )
}
