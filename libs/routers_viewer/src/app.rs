use std::{cell::RefCell, path::PathBuf};

use anyhow::Context as _;
use eframe::CreationContext;
use egui::{Color32, SidePanel};
use routers::{MatchError, RoutedPath};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{SYDNEY, fixture};
use walkers::{
    HttpTiles, MapMemory, Plugin,
    sources::{Mapbox, MapboxStyle, OpenStreetMap},
    lon_lat,
};

use crate::{
    ColourFactory, Component, Context, Input, Map, Regular, Results, Shell,
    plugins::LineStringPlugin,
};

const FIXTURE_NETWORK: &'static str = "fixture-network";
const MAPBOX_API_KEY: &'static str = "mapbox-api-key";

pub struct Application {
    network: OsmNetwork,
    map: Map,

    input_string: RefCell<String>,

    /// Last successful match result, retained across frames.
    match_cache: RefCell<Option<RoutedPath<OsmEntryId, OsmEdgeMetadata>>>,
    /// Error from the most recent failed match attempt.
    error_msg: RefCell<Option<String>>,
    /// Which discretized point the user has selected in the Results panel.
    selected_point: RefCell<Option<usize>>,
}

impl Application {
    pub fn new(ctx: &CreationContext<'_>) -> anyhow::Result<Self> {
        let storage = ctx
            .storage
            .context("was not compiled with storage feature")?;

        let api_key = storage
            .get_string(MAPBOX_API_KEY)
            .context("could not find mapbox API key")
            .ok();

        let default_path = fixture!(SYDNEY).clone();
        let path = storage
            .get_string(FIXTURE_NETWORK)
            .map(|v| PathBuf::from(v))
            .unwrap_or(default_path);

        path.try_exists()
            .context(path.to_string_lossy().to_string())
            .context("The path must point to a valid file.")?;

        log::info!("Opening road network at {:?}...", path);
        let network = OsmNetwork::from_pbf(&path).map_err(|e| anyhow::anyhow!("{}", e))?;

        let egui_ctx = ctx.egui_ctx.clone();
        let tiles = match api_key {
            Some(key) => HttpTiles::new(
                Mapbox {
                    style: MapboxStyle::Light,
                    high_resolution: true,
                    access_token: key,
                },
                egui_ctx,
            ),
            None => HttpTiles::new(OpenStreetMap, egui_ctx),
        };

        let memory = MapMemory::default();
        let center = lon_lat(151.12, -33.52);
        let map = Map::new(tiles, memory, center);

        Ok(Self {
            map,
            network,
            input_string: RefCell::new(String::new()),
            match_cache: RefCell::new(None),
            error_msg: RefCell::new(None),
            selected_point: RefCell::new(None),
        })
    }

    fn build_map_plugins(&self) -> Vec<Box<dyn Plugin + 'static>> {
        let cache = self.match_cache.borrow();
        let Some(path) = cache.as_ref() else {
            return vec![];
        };

        let mut plugins: Vec<Box<dyn Plugin + 'static>> = vec![];

        // Interpolated path in blue (full road geometry).
        let interp_coords: Vec<_> = path
            .interpolated
            .elements
            .iter()
            .map(|e| e.point)
            .collect();

        if !interp_coords.is_empty() {
            plugins.push(Box::new(
                LineStringPlugin::new(interp_coords)
                    .color(Color32::from_rgba_unmultiplied(0, 100, 255, 180))
                    .stroke_width(4.0),
            ));
        }

        // Discretized path in red (one point per input GPS point).
        let disc_coords: Vec<_> = path
            .discretized
            .elements
            .iter()
            .map(|e| e.point)
            .collect();

        if !disc_coords.is_empty() {
            plugins.push(Box::new(
                LineStringPlugin::new(disc_coords)
                    .color(Color32::from_rgba_unmultiplied(220, 50, 50, 200))
                    .stroke_width(2.0),
            ));
        }

        plugins
    }
}

impl eframe::App for Application {
    fn update(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        let scheme = ColourFactory::get_scheme(ctx.theme());
        let context = Context {
            scheme,
            layout: Box::new(Regular),
        };

        SidePanel::left("controls").show(ctx, |ui| {
            ui.heading("Routers Map Matcher");
            ui.separator();

            let input = Input::new(&self.input_string);
            let (_, result) = Shell::new(&self.network, &input).draw(&context, ui);

            match result {
                Ok(path) => {
                    // Center the map on the first matched point when a new
                    // result arrives.
                    if let Some(first) = path.discretized.elements.first() {
                        self.map.center_at(lon_lat(first.point.x, first.point.y));
                    }
                    *self.match_cache.borrow_mut() = Some(path);
                    *self.error_msg.borrow_mut() = None;
                    *self.selected_point.borrow_mut() = None;
                }
                Err(MatchError::NoPointsProvided) => {}
                Err(e) => {
                    *self.error_msg.borrow_mut() = Some(e.to_string());
                }
            }

            // Error display (task 3) — shown inline below the input/match area.
            if let Some(msg) = self.error_msg.borrow().as_deref() {
                ui.colored_label(Color32::RED, msg);
            }

            // Draw feature sidebar (task 1).
            let cache = self.match_cache.borrow();
            if let Some(path) = cache.as_ref() {
                ui.separator();
                Results::new(path, &self.selected_point).draw(&context, ui);
            }
        });

        // Build plugins from the cached match and hand them to the Map
        // before drawing — they are consumed each frame (task 4).
        self.map.set_plugins(self.build_map_plugins());

        egui::CentralPanel::default().show(ctx, |ui| self.map.draw(&context, ui));
    }
}
