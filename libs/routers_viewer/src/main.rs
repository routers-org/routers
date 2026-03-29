use eframe::{App, Frame, NativeOptions, egui};
use egui::{CentralPanel, Color32, Context, SidePanel, Stroke, TextEdit};
use walkers::{
    HttpTiles, Map, MapMemory, Plugin, Position, Projector, lon_lat, sources::OpenStreetMap,
};

use geo::{LineString, Coord};
use std::path::Path;

use routers::transition::candidate::collapse::CollapsedPath;
use routers::transition::candidate::{Candidate, CandidateId};
use routers::transition::costing::CostingStrategies;
use routers::transition::entity::Transition;
use routers::transition::layer::generation::StandardGenerator;
use routers::transition::solver::selective_forward::SelectiveForwardSolver;
use routers_codec::osm::{OsmEntryId, OsmNetwork, OsmTripConfiguration};
use routers_fixtures::{fixture, LOS_ANGELES, VENTURA_TRIP};
use routers_network::Network;

struct MatchState {
    original_line: LineString,
    collapsed: CollapsedPath<OsmEntryId>,
    interpolated_line: LineString,
}

struct ViewerApp {
    tiles: HttpTiles,
    map_memory: MapMemory,
    network: OsmNetwork,
    wkt_input: String,

    match_state: Option<MatchState>,
    selected_layer: Option<usize>,
    selected_candidate: Option<CandidateId>,
    hovered_transition: Option<(CandidateId, CandidateId)>,
    error_msg: Option<String>,
}

impl ViewerApp {
    fn new(egui_ctx: Context) -> Self {
        let path = Path::new(fixture!(LOS_ANGELES))
            .as_os_str()
            .to_ascii_lowercase();

        let network = OsmNetwork::new(path).expect("Graph must be created");

        // Default LineString from VENTURA_TRIP fixture
        let default_wkt = VENTURA_TRIP;

        Self {
            tiles: HttpTiles::new(OpenStreetMap, egui_ctx),
            map_memory: MapMemory::default(),
            network,
            wkt_input: default_wkt.to_string(),
            match_state: None,
            selected_layer: None,
            selected_candidate: None,
            hovered_transition: None,
            error_msg: None,
        }
    }

    fn perform_match(&mut self) {
        self.error_msg = None;
        let line: LineString = match wkt::TryFromWkt::try_from_wkt_str(&self.wkt_input) {
            Ok(l) => l,
            Err(e) => {
                self.error_msg = Some(format!("Invalid WKT: {}", e));
                return;
            }
        };

        let costing = CostingStrategies::default();
        let generator = StandardGenerator::new(&self.network, &costing.emission, 100.0);

        let transition = Transition::new(&self.network, line.clone(), &costing, generator);

        let solver = SelectiveForwardSolver::default();
        let runtime = OsmTripConfiguration::default();

        match transition.solve(solver, &runtime) {
            Ok(collapsed) => {
                let interpolated_line = collapsed.interpolated(&self.network);

                self.match_state = Some(MatchState {
                    original_line: line,
                    collapsed,
                    interpolated_line,
                });
                self.selected_layer = None;

                // Center map on the first point
                if let Some(pt) = self.match_state.as_ref().unwrap().original_line.0.first() {
                    self.map_memory.center_at(lon_lat(pt.x, pt.y));
                }
            }
            Err(e) => {
                self.error_msg = Some(format!("Match error: {:?}", e));
            }
        }
    }
}

impl App for ViewerApp {
    fn update(&mut self, ctx: &Context, _frame: &mut Frame) {
        SidePanel::left("controls").show(ctx, |ui| {
            ui.heading("Routers Map Matcher");
            ui.separator();

            ui.label("Input WKT (LineString):");
            ui.add(TextEdit::singleline(&mut self.wkt_input).desired_width(f32::INFINITY));

            if ui.button("Match").clicked() {
                self.perform_match();
            }

            if let Some(err) = &self.error_msg {
                ui.colored_label(Color32::RED, err);
            }

            ui.separator();

            if let Some(state) = &self.match_state {
                ui.heading("Match Results");
                ui.label(format!("Total Path Cost: {}", state.collapsed.cost));

                // Find all layers
                let mut max_layer = 0;
                let mut layer_counts = std::collections::HashMap::new();

                state.collapsed.candidates.lookup.scan(|_, v| {
                    let lid = v.location.layer_id;
                    max_layer = max_layer.max(lid);
                    *layer_counts.entry(lid).or_insert(0) += 1;
                });

                egui::ScrollArea::vertical().show(ui, |ui| {
                    for i in 0..=max_layer {
                        let count = layer_counts.get(&i).copied().unwrap_or(0);
                        let text = format!("Layer {} ({} candidates)", i, count);
                        let is_selected = self.selected_layer == Some(i);
                        if ui.selectable_label(is_selected, text).clicked() {
                            self.selected_layer = Some(i);
                        }
                    }
                });

                if let Some(layer_id) = self.selected_layer {
                    ui.separator();
                    ui.heading(format!("Layer {} Candidates", layer_id));

                    let mut layer_candidates = Vec::new();
                    state.collapsed.candidates.lookup.scan(|k, v| {
                        if v.location.layer_id == layer_id {
                            layer_candidates.push((*k, *v));
                        }
                    });
                    layer_candidates.sort_by_key(|(id, _)| id.index());

                    egui::ScrollArea::vertical()
                        .id_salt("candidates_list")
                        .max_height(200.0)
                        .show(ui, |ui| {
                            for (id, _cand) in &layer_candidates {
                                let is_chosen = state.collapsed.route.get(layer_id) == Some(id);
                                let is_selected = self.selected_candidate == Some(*id);
                                let text = format!(
                                    "Candidate {:?}{}",
                                    id,
                                    if is_chosen { " [CHOSEN]" } else { "" }
                                );
                                if ui.selectable_label(is_selected || is_chosen, text).clicked() {
                                    self.selected_candidate = Some(*id);
                                }
                            }
                        });

                    if let Some(cand_id) = self.selected_candidate {
                        if let Some(cand) = state.collapsed.candidates.candidate(&cand_id) {
                            ui.separator();
                            ui.heading(format!("Candidate {:?} Details", cand_id));
                            ui.label(format!("Emission Cost: {}", cand.emission));

                            if let Some(prev_idx) = layer_id.checked_sub(1) {
                                if let Some(prev_id) = state.collapsed.route.get(prev_idx) {
                                    if let Some(edge) =
                                        state.collapsed.candidates.edge(prev_id, &cand_id)
                                    {
                                        ui.label(format!(
                                            "Transition Cost from Prev Chosen: {}",
                                            edge.weight
                                        ));
                                    }
                                }
                            }

                            #[cfg(debug_assertions)]
                            {
                                ui.separator();
                                ui.heading("Considered Transitions");
                                egui::ScrollArea::vertical()
                                    .id_salt("transitions_list")
                                    .show(ui, |ui| {
                                        ui.label("From previous layer:");
                                        for (reachable, cost) in &state.collapsed.considered {
                                            if reachable.target == cand_id {
                                                let text = format!("<- {:?}: Cost {}", reachable.source, cost);
                                                let is_hovered = self.hovered_transition == Some((reachable.source, cand_id));
                                                let resp = ui.selectable_label(is_hovered, text);
                                                if resp.hovered() {
                                                    self.hovered_transition = Some((reachable.source, cand_id));
                                                } else if is_hovered {
                                                    self.hovered_transition = None;
                                                }
                                            }
                                        }

                                        ui.separator();
                                        ui.label("To next layer:");
                                        for (reachable, cost) in &state.collapsed.considered {
                                            if reachable.source == cand_id {
                                                let text = format!("-> {:?}: Cost {}", reachable.target, cost);
                                                let is_hovered = self.hovered_transition == Some((cand_id, reachable.target));
                                                let resp = ui.selectable_label(is_hovered, text);
                                                if resp.hovered() {
                                                    self.hovered_transition = Some((cand_id, reachable.target));
                                                } else if is_hovered {
                                                    self.hovered_transition = None;
                                                }
                                            }
                                        }
                                    });
                            }
                        }
                    }
                }
            }
        });

        CentralPanel::default().show(ctx, |ui| {
            let mut map = Map::new(
                Some(&mut self.tiles),
                &mut self.map_memory,
                lon_lat(-118.49, 34.01), // Near Santa Monica/Ventura
            );

            if let Some(state) = &self.match_state {
                // Plugin to draw the lines
                let line_plugin = LinePlugin {
                    original: state.original_line.clone(),
                    interpolated: state.interpolated_line.clone(),
                };
                map = map.with_plugin(line_plugin);

                // Plugin to draw candidates
                if let Some(layer_id) = self.selected_layer {
                    let mut candidates = Vec::new();
                    let mut chosen_id = None;

                    if let Some(c_id) = state.collapsed.route.get(layer_id) {
                        chosen_id = Some(*c_id);
                    }

                    state.collapsed.candidates.lookup.scan(|k, v| {
                        if v.location.layer_id == layer_id {
                            candidates.push((*k, *v));
                        }
                    });

                    let original_coord = state.original_line.0.get(layer_id).copied();

                    let cand_plugin = CandidatePlugin {
                        candidates,
                        chosen_id,
                        selected_id: self.selected_candidate,
                        original_coord,
                    };
                    map = map.with_plugin(cand_plugin);
                }

                // Plugin to draw hovered transition
                #[cfg(debug_assertions)]
                {
                    if let Some((src_id, dst_id)) = self.hovered_transition {
                        for (reachable, _) in &state.collapsed.considered {
                            if reachable.source == src_id && reachable.target == dst_id {
                                let mut pts = Vec::new();
                                if let Some(src) = state.collapsed.candidates.candidate(&src_id) {
                                    pts.push(src.position.0);
                                }
                                for edge in &reachable.path {
                                    if let Some(p) = self.network.point(&edge.source) {
                                        pts.push(p.0);
                                    }
                                    if let Some(p) = self.network.point(&edge.target) {
                                        pts.push(p.0);
                                    }
                                }
                                if let Some(dst) = state.collapsed.candidates.candidate(&dst_id) {
                                    pts.push(dst.position.0);
                                }

                                map = map.with_plugin(TransitionPlugin {
                                    pts,
                                    color: Color32::YELLOW,
                                    weight: 6.0,
                                });
                                break;
                            }
                        }
                    }
                }
            }

            ui.add(map);
        });
    }
}

struct LinePlugin {
    original: LineString,
    interpolated: LineString,
}

impl Plugin for LinePlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();

        // Draw interpolated match as BLUE
        if !self.interpolated.0.is_empty() {
            let pts: Vec<_> = self
                .interpolated
                .0
                .iter()
                .map(|p| projector.project(lon_lat(p.x, p.y)).to_pos2())
                .collect();
            painter.line(pts, Stroke::new(4.0, Color32::from_rgb(0, 100, 255)));
        }

        // Draw original WKT as RED (faded)
        if !self.original.0.is_empty() {
            let pts: Vec<_> = self
                .original
                .0
                .iter()
                .map(|p| projector.project(lon_lat(p.x, p.y)).to_pos2())
                .collect();
            painter.line(
                pts,
                Stroke::new(2.0, Color32::from_rgba_unmultiplied(255, 0, 0, 100)),
            );
        }
    }
}

struct TransitionPlugin {
    pts: Vec<Coord>,
    color: Color32,
    weight: f32,
}

impl Plugin for TransitionPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();
        if self.pts.len() < 2 {
            return;
        }

        let pts: Vec<_> = self
            .pts
            .iter()
            .map(|p| projector.project(lon_lat(p.x, p.y)).to_pos2())
            .collect();

        painter.line(pts, Stroke::new(self.weight, self.color));
    }
}

struct CandidatePlugin {
    candidates: Vec<(CandidateId, Candidate<OsmEntryId>)>,
    chosen_id: Option<CandidateId>,
    selected_id: Option<CandidateId>,
    original_coord: Option<Coord>,
}

impl Plugin for CandidatePlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();

        // Draw the original input point in RED
        if let Some(coord) = self.original_coord {
            let pos = projector.project(lon_lat(coord.x, coord.y)).to_pos2();
            painter.circle_filled(pos, 8.0, Color32::RED);
            painter.circle_stroke(pos, 8.0, Stroke::new(2.0, Color32::BLACK));
            painter.text(
                pos - egui::vec2(0.0, 15.0),
                egui::Align2::CENTER_BOTTOM,
                "Original",
                egui::FontId::proportional(12.0),
                Color32::RED,
            );
        }

        for (id, cand) in &self.candidates {
            let pos = projector
                .project(lon_lat(cand.position.x(), cand.position.y()))
                .to_pos2();

            let is_chosen = Some(*id) == self.chosen_id;
            let is_selected = Some(*id) == self.selected_id;
            
            let color = if is_selected {
                Color32::from_rgb(255, 165, 0) // Orange
            } else if is_chosen {
                Color32::BLUE
            } else {
                Color32::GRAY
            };
            
            let radius = if is_selected || is_chosen { 8.0 } else { 5.0 };

            painter.circle_filled(pos, radius, color);
            painter.circle_stroke(pos, radius, Stroke::new(1.0, Color32::BLACK));

            // Draw individual cost (Emission)
            let text = format!("{}", cand.emission);
            painter.text(
                pos + egui::vec2(8.0, 8.0),
                egui::Align2::LEFT_TOP,
                text,
                egui::FontId::proportional(12.0),
                if is_chosen {
                    Color32::BLUE
                } else if is_selected {
                    Color32::from_rgb(255, 165, 0)
                } else {
                    Color32::DARK_GRAY
                },
            );

            if is_chosen {
                painter.text(
                    pos - egui::vec2(0.0, 15.0),
                    egui::Align2::CENTER_BOTTOM,
                    "Match",
                    egui::FontId::proportional(12.0),
                    Color32::BLUE,
                );
            }
        }
    }
}

#[tokio::main]
async fn main() -> eframe::Result<()> {
    env_logger::init();
    let native_options = NativeOptions::default();
    eframe::run_native(
        "Routers Map Matcher",
        native_options,
        Box::new(|cc| Ok(Box::new(ViewerApp::new(cc.egui_ctx.clone())))),
    )
}
