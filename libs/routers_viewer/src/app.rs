use std::{cell::RefCell, path::PathBuf};

use anyhow::Context as _;
use eframe::CreationContext;
use egui::{Color32, CursorIcon, SidePanel};
use geo::Coord;
use routers_codec::osm::OsmNetwork;
use routers_fixtures::{SYDNEY, fixture};
use walkers::{
    HttpTiles, MapMemory, Plugin, lon_lat,
    sources::{Mapbox, MapboxStyle, OpenStreetMap},
};
use wkt::ToWkt as _;

use crate::{
    ColourFactory, Component, Context, Input, Map, MatchData, Matcher, Regular, Results, Stack,
    plugins::{CandidatesPlugin, ChosenPathPlugin, DrawPlugin, LineStringPlugin},
};

const FIXTURE_NETWORK: &'static str = "fixture-network";
const MAPBOX_API_KEY: &'static str = "mapbox-api-key";

pub struct Application {
    network: OsmNetwork,
    map: Map,
    input_string: RefCell<String>,
    match_cache: RefCell<Option<MatchData>>,
    error_msg: RefCell<Option<String>>,
    selected_layer: RefCell<Option<usize>>,
    selected_candidate: RefCell<Option<usize>>,
    draw_mode: RefCell<bool>,
    drawn_points: RefCell<Vec<Coord>>,
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

        let map = Map::new(tiles, MapMemory::default(), lon_lat(151.12, -33.52));

        Ok(Self {
            map,
            network,
            input_string: RefCell::new(String::new()),
            match_cache: RefCell::new(None),
            error_msg: RefCell::new(None),
            selected_layer: RefCell::new(None),
            selected_candidate: RefCell::new(None),
            draw_mode: RefCell::new(false),
            drawn_points: RefCell::new(Vec::new()),
        })
    }

    fn build_map_plugins(&self) -> Vec<Box<dyn Plugin + 'static>> {
        let mut plugins: Vec<Box<dyn Plugin + 'static>> = vec![];

        if *self.draw_mode.borrow() {
            let pts = self.drawn_points.borrow().clone();
            if !pts.is_empty() {
                plugins.push(Box::new(DrawPlugin { points: pts }));
            }
        }

        let cache = self.match_cache.borrow();
        let Some(data) = cache.as_ref() else {
            return plugins;
        };

        let orig_coords: Vec<_> = data.original_line.0.iter().copied().collect();
        if orig_coords.len() >= 2 {
            plugins.push(Box::new(
                LineStringPlugin::new(orig_coords)
                    .color(Color32::from_rgba_unmultiplied(220, 50, 50, 160))
                    .stroke_width(2.0),
            ));
        }

        let interp_coords: Vec<_> = data.interpolated_line.0.iter().copied().collect();
        if interp_coords.len() >= 2 {
            plugins.push(Box::new(
                LineStringPlugin::new(interp_coords)
                    .color(Color32::from_rgba_unmultiplied(0, 100, 255, 200))
                    .stroke_width(4.0),
            ));
        }

        plugins.push(Box::new(ChosenPathPlugin {
            layers: data.layers.clone(),
        }));

        if let Some(layer_idx) = *self.selected_layer.borrow() {
            if layer_idx > 0 {
                if let Some(coords) = data.transitions.get(layer_idx - 1) {
                    if coords.len() >= 2 {
                        plugins.push(Box::new(
                            LineStringPlugin::new(coords.clone())
                                .color(Color32::YELLOW)
                                .stroke_width(5.0),
                        ));
                    }
                }
            }

            if let Some(coords) = data.transitions.get(layer_idx) {
                if coords.len() >= 2 {
                    plugins.push(Box::new(
                        LineStringPlugin::new(coords.clone())
                            .color(Color32::from_rgb(255, 140, 0))
                            .stroke_width(5.0),
                    ));
                }
            }

            if let Some(layer) = data.layers.get(layer_idx) {
                plugins.push(Box::new(CandidatesPlugin {
                    layer: layer.clone(),
                    selected_idx: *self.selected_candidate.borrow(),
                }));
            }
        }

        plugins
    }

    fn commit_drawn_point(&self, coord: Coord) {
        self.drawn_points.borrow_mut().push(coord);
        let pts = self.drawn_points.borrow();
        if pts.len() >= 2 {
            *self.input_string.borrow_mut() = geo::LineString::new(pts.clone()).wkt_string();
        }
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
            let (_, linestring) = input.draw(&context, ui);

            ui.horizontal(|ui| {
                let drawing = *self.draw_mode.borrow();

                if ui.selectable_label(drawing, "✏  Draw").clicked() {
                    let new_mode = !drawing;
                    *self.draw_mode.borrow_mut() = new_mode;
                    if new_mode {
                        self.drawn_points.borrow_mut().clear();
                    }
                }

                if ui.button("✕  Clear").clicked() {
                    *self.input_string.borrow_mut() = String::new();
                    *self.drawn_points.borrow_mut() = Vec::new();
                    *self.draw_mode.borrow_mut() = false;
                    *self.match_cache.borrow_mut() = None;
                    *self.selected_layer.borrow_mut() = None;
                    *self.selected_candidate.borrow_mut() = None;
                    *self.error_msg.borrow_mut() = None;
                }
            });

            if *self.draw_mode.borrow() {
                let n = self.drawn_points.borrow().len();
                let hint = if n == 0 {
                    "Click the map to add points".to_owned()
                } else {
                    format!(
                        "{n} point{} — click to add more",
                        if n == 1 { "" } else { "s" }
                    )
                };
                ui.colored_label(ctx.theme().default_visuals().warn_fg_color, hint);
            }

            ui.separator();

            let matcher = Matcher::new(&self.network, linestring);
            let (_, result) = Stack::new(&matcher).draw(&context, ui);

            match result {
                None => {}
                Some(Ok(data)) => {
                    if let Some(first) = data.layers.first() {
                        self.map
                            .center_at(lon_lat(first.original.x, first.original.y));
                    }
                    *self.match_cache.borrow_mut() = Some(data);
                    *self.error_msg.borrow_mut() = None;
                    *self.selected_layer.borrow_mut() = None;
                    *self.selected_candidate.borrow_mut() = None;
                }
                Some(Err(msg)) => {
                    *self.error_msg.borrow_mut() = Some(msg);
                }
            }

            if let Some(msg) = self.error_msg.borrow().as_deref() {
                ui.colored_label(Color32::RED, msg);
            }

            let cache = self.match_cache.borrow();
            if let Some(data) = cache.as_ref() {
                ui.separator();
                Results::new(data, &self.selected_layer, &self.selected_candidate)
                    .draw(&context, ui);
            }
        });

        self.map.set_plugins(self.build_map_plugins());

        egui::CentralPanel::default().show(ctx, |ui| {
            let (response, _) = self.map.draw(&context, ui);

            if *self.draw_mode.borrow() {
                if response.hovered() {
                    ctx.set_cursor_icon(CursorIcon::Crosshair);
                }
                if response.clicked() {
                    if let Some(screen_pos) = response.interact_pointer_pos() {
                        let projector = self.map.projector(response.rect);
                        let geo = projector.unproject(screen_pos.to_vec2());
                        self.commit_drawn_point(Coord {
                            x: geo.x(),
                            y: geo.y(),
                        });
                    }
                }
            }
        });
    }
}
