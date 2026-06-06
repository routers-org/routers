use std::path::PathBuf;

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

use crate::{
    ColourFactory, Component, Context, Input, Map, MatchCache, Matcher, Regular, Results, Stack,
    State,
    plugins::{CandidatesPlugin, ChosenPathPlugin, DrawPlugin, LineStringPlugin},
};

const FIXTURE_NETWORK: &'static str = "fixture-network";
const MAPBOX_API_KEY: &'static str = "mapbox-api-key";

pub struct Application {
    network: OsmNetwork,
    map: Map,
    solver_cache: MatchCache,
    state: State,
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

        Ok(Self {
            map: Map::new(tiles, MapMemory::default(), lon_lat(151.12, -33.52)),
            network,
            solver_cache: std::sync::Arc::new(Default::default()),
            state: State::default(),
        })
    }

    fn build_map_plugins(&self) -> Vec<Box<dyn Plugin + 'static>> {
        let mut plugins: Vec<Box<dyn Plugin + 'static>> = vec![];

        if self.state.draw.is_active() {
            plugins.push(Box::new(DrawPlugin {
                points: self.state.draw.points.borrow().clone(),
                cursor: self.state.cursor.pos(),
            }));
        }

        let preview = self.state.cursor.preview.borrow();
        let confirmed = self.state.result.data.borrow();
        let Some(data) = preview.as_ref().or(confirmed.as_ref()) else {
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

        if let Some(layer_idx) = *self.state.selection.layer.borrow() {
            if let Some(layer) = data.layers.get(layer_idx) {
                plugins.push(Box::new(CandidatesPlugin {
                    layer: layer.clone(),
                    selected_idx: *self.state.selection.candidate.borrow(),
                }));
            }
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

            let input = Input::new(&self.state.input);
            let (_, linestring) = input.draw(&context, ui);

            ui.horizontal(|ui| {
                let drawing = self.state.draw.is_active();

                if ui.selectable_label(drawing, "✏  Draw").clicked() {
                    if drawing {
                        self.state.exit_draw_mode();
                    } else {
                        self.state.draw.enter();
                    }
                }

                if ui.button("✕  Clear").clicked() {
                    self.state = State::default();
                }
            });

            if self.state.draw.is_active() {
                let n = self.state.draw.points.borrow().len();
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

            let matcher = Matcher::new(&self.network, linestring, self.solver_cache.clone()).drawn(
                self.state.draw.points.borrow().clone(),
                self.state.cursor.pos(),
            );
            let (_, output) = Stack::new(&matcher).draw(&context, ui);

            self.state.cursor.set_preview(output.preview);

            match output.confirmed {
                None => {}
                Some(Ok(data)) => {
                    if let Some(first) = data.layers.first() {
                        self.map
                            .center_at(lon_lat(first.original.x, first.original.y));
                    }
                    self.state.result.set(data);
                    self.state.selection.clear();
                }
                Some(Err(msg)) => self.state.result.set_error(msg),
            }

            if let Some(msg) = self.state.result.error.borrow().as_deref() {
                ui.colored_label(Color32::RED, msg);
            }

            let preview = self.state.cursor.preview.borrow();
            let confirmed = self.state.result.data.borrow();
            if let Some(data) = preview.as_ref().or(confirmed.as_ref()) {
                ui.separator();
                Results::new(
                    data,
                    &self.state.selection.layer,
                    &self.state.selection.candidate,
                )
                .draw(&context, ui);
            }
        });

        self.map.set_plugins(self.build_map_plugins());

        egui::CentralPanel::default().show(ctx, |ui| {
            let (response, _) = self.map.draw(&context, ui);

            if self.state.draw.is_active() {
                if response.hovered() {
                    ctx.set_cursor_icon(CursorIcon::Crosshair);
                }

                if let Some(screen_pos) = response.hover_pos() {
                    let projector = self.map.projector(response.rect);
                    let geo = projector.unproject(screen_pos.to_vec2());
                    self.state.cursor.set(Coord {
                        x: geo.x(),
                        y: geo.y(),
                    });
                } else {
                    self.state.cursor.clear();
                }

                if response.double_clicked() {
                    self.state.exit_draw_mode();
                } else if response.clicked() {
                    if let Some(screen_pos) = response.interact_pointer_pos() {
                        let projector = self.map.projector(response.rect);
                        let geo = projector.unproject(screen_pos.to_vec2());
                        self.state.commit_drawn_point(Coord {
                            x: geo.x(),
                            y: geo.y(),
                        });
                    }
                }
            }
        });
    }
}
