use walkers::{
    HttpTiles,
    sources::{Mapbox, MapboxStyle, OpenStreetMap},
};

pub const MAPBOX_API_KEY: &str = "mapbox-api-key";

/// Build the tile source shared by all viewer binaries: Mapbox when an API key
/// is present in eframe storage, OpenStreetMap otherwise.
pub fn tile_source(storage: Option<&dyn eframe::Storage>, egui_ctx: egui::Context) -> HttpTiles {
    let api_key = storage.and_then(|s| s.get_string(MAPBOX_API_KEY));

    match api_key {
        Some(key) => HttpTiles::new(
            Mapbox {
                style: MapboxStyle::Light,
                high_resolution: true,
                access_token: key,
            },
            egui_ctx,
        ),
        None => HttpTiles::new(OpenStreetMap, egui_ctx),
    }
}
