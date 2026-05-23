use eframe::{App, Frame, egui};
use egui::{CentralPanel, Color32, Context, Response, SidePanel, Stroke, TextEdit, Widget};
use walkers::{
    HttpTiles, Map, MapMemory, Plugin, Position, Projector, lon_lat, sources::OpenStreetMap,
};

use geo::{Coord, LineString};
use routers::Trip;
use routers::transition::candidate::collapse::CollapsedPath;
use routers::transition::candidate::{Candidate, CandidateId};
use routers::transition::costing::CostingStrategies;
use routers::transition::entity::Transition;
use routers::transition::layer::generation::StandardGenerator;
use routers::transition::solver::selective_forward::SelectiveForwardSolver;
use routers_network::{DataPlane, Discovery, Entry, Metadata, Node, Route, Scan};
use std::marker::PhantomData;
use std::time::Duration;
// `web_time::Instant` is a drop-in for `std::time::Instant` that doesn't
// panic on `wasm32-unknown-unknown`. `Duration` is pure arithmetic so the
// std version is portable.
use web_time::Instant;
use wkt::ToWkt;

struct MatchState<E: Entry> {
    original_line: LineString,
    collapsed: CollapsedPath<E>,
    interpolated_line: LineString,
    discrete_line: LineString,
    time: Duration,
}

/// Generic over any routing network — bound on [`DataPlane`] (with the
/// usual `Discovery + Scan + Route` companions) so we can flip between
/// `OsmNetwork`, `ShardedNetwork<…>` or any other implementor without
/// touching the viewer code. The Entry / Metadata types fall out of
/// `N::Entry` / `N::Meta` via the `DataPlane` associated types — callers
/// supply only `N`.
pub struct ViewerApp<N>
where
    N: DataPlane,
{
    tiles: HttpTiles,
    map_memory: MapMemory,
    /// Fallback centre used by `Map::new` when the map isn't currently
    /// detached (i.e. the user hasn't panned manually). Lets the host
    /// configure where the map "starts" instead of relying on a baked-in
    /// constant. The same value is also what
    /// [`current_center`](Self::current_center) returns when the map is
    /// in "my position" mode.
    my_position: Position,
    network: N,
    wkt_input: String,

    match_state: Option<MatchState<<N as DataPlane>::Entry>>,
    selected_layer: Option<usize>,
    selected_candidate: Option<CandidateId>,
    hovered_transition: Option<(CandidateId, CandidateId)>,
    error_msg: Option<String>,

    // ── Draw tool ───────────────────────────────────────────────────────
    /// When `true`, map clicks add waypoints to `drawn_path` instead of
    /// just being absorbed by walkers' pan/zoom.
    draw_mode: bool,
    /// Live-updated polyline the user is drawing. Clicks append to it;
    /// the debounced background match runs against this directly.
    drawn_path: Vec<Coord>,
    /// Cursor position projected to lat/lon while drawing — rendered as
    /// a faded segment from the last waypoint, so the user sees where
    /// the next click would land.
    hover_preview: Option<Coord>,
    /// Last time `drawn_path` changed; the match re-runs once this is
    /// older than [`DRAW_DEBOUNCE`] and the path differs from the
    /// previously-matched input.
    last_draw_change: Option<Instant>,
}

/// How long to wait after the most recent waypoint edit before re-running
/// the matcher. Picked so a steady stream of clicks coalesces into a
/// single match per gesture, while a single drop still lands within a
/// quarter-second.
const DRAW_DEBOUNCE: Duration = Duration::from_millis(250);

impl<N> ViewerApp<N>
where
    N: DataPlane
        + Discovery<<N as DataPlane>::Entry>
        + Scan<<N as DataPlane>::Entry>
        + Route<<N as DataPlane>::Entry>,
{
    /// Default initial centre — central Sydney. Override via
    /// [`new_at`](Self::new_at).
    pub fn new(egui_ctx: Context, network: N) -> Self {
        Self::new_at(egui_ctx, network, lon_lat(151.2, -33.8))
    }

    /// Construct with an explicit initial map position (used until the
    /// user pans the map manually).
    pub fn new_at(egui_ctx: Context, network: N, my_position: Position) -> Self {
        // Default tile source is OpenStreetMap's standard raster pyramid —
        // no API key, works the same on native and WASM. If you want a
        // different style (e.g. Mapbox), construct the `HttpTiles` yourself
        // and pass it in via a constructor variant.
        Self {
            tiles: HttpTiles::new(OpenStreetMap, egui_ctx),
            map_memory: MapMemory::default(),
            my_position,
            network,
            wkt_input: "WKT Here...".into(),
            match_state: None,
            selected_layer: None,
            selected_candidate: None,
            hovered_transition: None,
            error_msg: None,
            draw_mode: false,
            drawn_path: Vec::new(),
            hover_preview: None,
            last_draw_change: None,
        }
    }

    /// Swap in a new network without losing UI state (input, selected
    /// layer, etc.). Clears `match_state` because the previous match's
    /// node ids are no longer guaranteed to resolve against the new
    /// network.
    pub fn set_network(&mut self, network: N) {
        self.network = network;
        self.match_state = None;
        self.selected_layer = None;
        self.selected_candidate = None;
        self.hovered_transition = None;
    }

    /// Read-only handle to the underlying network.
    pub fn network(&self) -> &N {
        &self.network
    }

    /// The map's current effective centre — `detached()` if the user has
    /// panned, otherwise the configured `my_position`.
    pub fn current_center(&self) -> Position {
        self.map_memory.detached().unwrap_or(self.my_position)
    }

    /// Inspect the internal `MapMemory` (zoom, detached state). Useful
    /// for shard-window drivers that need finer detail than
    /// [`current_center`](Self::current_center) provides.
    pub fn map_memory(&self) -> &MapMemory {
        &self.map_memory
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
        self.match_linestring(line, /* recenter = */ true);
    }

    /// Run the matcher against a pre-parsed `LineString`. Used by both
    /// the manual WKT button and the debounced draw-mode loop.
    ///
    /// `recenter` controls whether to re-centre the map on the first
    /// point — fine for the explicit Match button (one-shot), but
    /// disruptive when called every 250 ms from the draw loop because
    /// the user is *currently looking at* the area they're drawing.
    fn match_linestring(&mut self, line: LineString, recenter: bool) {
        if line.0.len() < 2 {
            // The matcher requires at least two points; drop silently
            // rather than spam an error from the debounce loop.
            self.error_msg = None;
            self.match_state = None;
            return;
        }
        self.error_msg = None;

        let costing = CostingStrategies::default();
        let generator = StandardGenerator::new(&self.network, &costing.emission, 100.0);
        let transition = Transition::new(&self.network, line.clone(), &costing, generator);

        let solver = SelectiveForwardSolver::default();
        // Use the metadata-defined default runtime instead of hard-coding
        // `OsmTripConfiguration::default()` — generic over `N::Meta`.
        let runtime = <<N as DataPlane>::Meta as Metadata>::default_runtime();

        let now = Instant::now();

        match transition.solve(solver, &runtime) {
            Ok(collapsed) => {
                let interpolated_line = collapsed.interpolated(&self.network);
                let discrete_line = collapsed.collapsed();

                self.match_state = Some(MatchState {
                    original_line: line,
                    collapsed,
                    interpolated_line,
                    discrete_line,
                    time: now.elapsed(),
                });

                self.selected_layer = None;

                if recenter {
                    if let Some(pt) = self.match_state.as_ref().unwrap().original_line.0.first() {
                        self.map_memory.center_at(lon_lat(pt.x, pt.y));
                    }
                }
            }
            Err(e) => {
                self.error_msg = Some(format!("Match error: {:?}", e));
            }
        }
    }
}

struct TransitionWidget<E: Entry> {
    cost: u32,

    #[allow(dead_code)]
    source: Candidate<E>,
    target: Candidate<E>,

    nodes: Vec<Node<E>>,
}

impl<E: Entry> Widget for TransitionWidget<E> {
    fn ui(self, ui: &mut egui::Ui) -> Response {
        let trip = Trip::from(self.nodes);
        let linestring = trip.linestring();

        ui.horizontal(|ui| {
            let label = ui.selectable_label(
                false,
                format!(
                    "-> {}: Cost {}, R={:.1}m, S={:.1}m",
                    self.target.edge.id().identifier(),
                    self.cost,
                    trip.length(),
                    trip.straightline_length()
                ),
            );

            if ui.button("L").clicked() {
                ui.ctx().copy_text(linestring.wkt_string());
            }

            label
        })
        .inner
    }
}

impl<N> App for ViewerApp<N>
where
    N: DataPlane
        + Discovery<<N as DataPlane>::Entry>
        + Scan<<N as DataPlane>::Entry>
        + Route<<N as DataPlane>::Entry>
        + 'static,
{
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

            // ── Draw tool ─────────────────────────────────────────────
            ui.checkbox(&mut self.draw_mode, "✏ Draw mode");
            if self.draw_mode {
                ui.label(format!(
                    "{} waypoint{}",
                    self.drawn_path.len(),
                    if self.drawn_path.len() == 1 { "" } else { "s" }
                ));
                ui.horizontal(|ui| {
                    if ui.button("Clear").clicked() {
                        self.drawn_path.clear();
                        self.hover_preview = None;
                        self.last_draw_change = None;
                        self.match_state = None;
                    }
                    if ui.button("→ WKT").clicked() && self.drawn_path.len() >= 2 {
                        let line: LineString = LineString::new(self.drawn_path.clone());
                        self.wkt_input = line.wkt_string();
                    }
                });
                ui.colored_label(
                    Color32::from_gray(160),
                    "Click on the map to add waypoints. \
                     Hold ⌘/Ctrl while dragging to pan instead of drawing.",
                );
            }

            ui.separator();

            if let Some(state) = &self.match_state {
                ui.heading("Match Results");
                ui.label(format!("Total Path Cost: {}", state.collapsed.cost));
                ui.label(format!("Time Taken: {}ms", state.time.as_millis()));

                // Find all layers
                let mut max_layer = 0;
                let mut layer_counts = std::collections::HashMap::new();

                state.collapsed.candidates.lookup.scan(|_, v| {
                    let lid = v.location.layer_id;
                    max_layer = max_layer.max(lid);
                    *layer_counts.entry(lid).or_insert(0) += 1;
                });

                egui::ScrollArea::vertical()
                    .max_height(200.0)
                    .show(ui, |ui| {
                        for i in 0..=max_layer {
                            let count = layer_counts.get(&i).copied().unwrap_or(0);
                            let text = format!("Layer {i} ({count} candidates)");

                            let is_selected = self.selected_layer.is_some_and(|v| v == i);

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
                    layer_candidates.sort_by_key(|(_, c)| c.emission);

                    egui::ScrollArea::vertical()
                        .id_salt("candidates_list")
                        .max_height(200.0)
                        .show(ui, |ui| {
                            for (id, cand) in &layer_candidates {
                                let is_chosen = state.collapsed.route.get(layer_id) == Some(id);
                                let is_selected = self.selected_candidate == Some(*id);

                                let text = format!(
                                    "Candidate {:?}\n Cost={}\n EdgeId={}\n EdgeWeight={}\nSource={}\nTarget={}",
                                    id,
                                    cand.emission,
                                    cand.edge.id.index().identifier(),
                                    cand.edge.weight,
                                    cand.edge.source.identifier(),
                                    cand.edge.target.identifier()
                                );

                                if ui
                                    .selectable_label(is_selected || is_chosen, text)
                                    .clicked()
                                {
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

                                        let considered = state
                                            .collapsed
                                            .considered
                                            .iter()
                                            .filter(|r| r.source == cand_id)
                                            .collect::<Vec<_>>();

                                        let mut normalized = considered.into_iter().map(|reachable| {
                                            (reachable, reachable.cost)
                                        }).collect::<Vec<_>>();
                                        normalized.sort_by_key(|v| v.1);

                                        normalized.into_iter().for_each(|(reachable, cost)| {
                                            if reachable.source == cand_id {
                                                let source = state
                                                    .collapsed
                                                    .candidates
                                                    .candidate(&reachable.source)
                                                    .unwrap();

                                                let target = state
                                                    .collapsed
                                                    .candidates
                                                    .candidate(&reachable.target)
                                                    .unwrap();

                                                let nodes = reachable.path
                                                    .iter()
                                                    .filter_map(|edge| {
                                                        let a = self.network.node(&edge.source);
                                                        let b = self.network.node(&edge.target);

                                                        if let (Some(a), Some(b)) = (a, b) {
                                                            return Some(vec![a, b]);
                                                        }

                                                        return None
                                                    })
                                                    .flatten()
                                                    .cloned()
                                                    .collect::<Vec<_>>();

                                                let hovered_transition = (reachable.source, cand_id);
                                                let is_hovered =
                                                    self.hovered_transition == Some(hovered_transition);

                                                let resp = ui.add(TransitionWidget::<<N as DataPlane>::Entry> {
                                                    cost,

                                                    source,
                                                    target,

                                                    nodes,
                                                });

                                                if resp.hovered() {
                                                    self.hovered_transition = Some(hovered_transition);
                                                } else if is_hovered {
                                                    self.hovered_transition = None;
                                                }
                                            }
                                        });

                                        ui.separator();
                                        ui.label("To next layer:");

                                        let considered = state
                                            .collapsed
                                            .considered
                                            .iter()
                                            .filter(|r| r.source == cand_id)
                                            .collect::<Vec<_>>();

                                        let mut normalized = considered.into_iter().map(|reachable| {
                                            let next_cand = state.collapsed.candidates.candidate(&reachable.target)
                                                .map_or(0, |v| v.emission);

                                            (reachable, reachable.cost - next_cand)
                                        }).collect::<Vec<_>>();

                                        normalized.sort_by_key(|v| v.1);
                                        normalized.into_iter().for_each(|(reachable, cost)| {
                                            let source = state
                                                .collapsed
                                                .candidates
                                                .candidate(&reachable.source)
                                                .unwrap();

                                            let target = state
                                                .collapsed
                                                .candidates
                                                .candidate(&reachable.target)
                                                .unwrap();

                                            let nodes = reachable.path
                                                .iter()
                                                .filter_map(|edge| {
                                                    let a = self.network.node(&edge.source);
                                                    let b = self.network.node(&edge.target);

                                                    if let (Some(a), Some(b)) = (a, b) {
                                                        return Some(vec![a, b]);
                                                    }

                                                    return None
                                                })
                                                .flatten()
                                                .cloned()
                                                .collect::<Vec<_>>();

                                            let hovered_transition = (cand_id, reachable.target);
                                            let is_hovered =
                                                self.hovered_transition == Some(hovered_transition);

                                            let resp = ui.add(TransitionWidget::<<N as DataPlane>::Entry> {
                                                cost,

                                                source,
                                                target,

                                                nodes,
                                            });

                                            if resp.hovered() {
                                                self.hovered_transition = Some(hovered_transition);
                                            } else if is_hovered {
                                                self.hovered_transition = None;
                                            }
                                        });
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
                self.my_position,
            );

            // While the draw tool is active, paint the in-flight path so
            // the user can see what they've laid down and where the next
            // click would land.
            if self.draw_mode {
                map = map.with_plugin(DrawPlugin {
                    path: self.drawn_path.clone(),
                    hover: self.hover_preview,
                });
            }

            if let Some(state) = &self.match_state {
                // Plugin to draw the lines
                let line_plugin = LinePlugin {
                    original: state.original_line.clone(),
                    interpolated: state.interpolated_line.clone(),
                    discrete: state.discrete_line.clone(),
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

                    let cand_plugin = CandidatePlugin::<<N as DataPlane>::Entry> {
                        candidates,
                        chosen_id,
                        selected_id: self.selected_candidate,
                        original_coord,
                        _ph: PhantomData,
                    };
                    map = map.with_plugin(cand_plugin);
                }

                // Plugin to draw hovered transition
                #[cfg(debug_assertions)]
                {
                    if let Some((src_id, dst_id)) = self.hovered_transition {
                        for reachable in &state.collapsed.considered {
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

            let response = ui.add(map);

            // Draw-mode interactions: rebuild a `Projector` post-frame so
            // we can unproject click/hover positions back to lat/lon. The
            // walkers `Plugin` API runs *during* widget construction and
            // can't write back to viewer state, so this is the
            // out-of-band hook.
            if self.draw_mode {
                let projector = Projector::new(
                    response.rect,
                    &self.map_memory,
                    self.current_center(),
                );

                // Hover preview: update every frame the cursor is over
                // the map. egui's `Pos2` is widget-relative? — projector
                // wants a `Vec2`, which walkers treats as the same
                // coordinate space.
                let new_hover = response.hover_pos().map(|pos| {
                    let pos = projector.unproject(egui::vec2(pos.x, pos.y));
                    Coord {
                        x: pos.x(),
                        y: pos.y(),
                    }
                });
                if new_hover != self.hover_preview {
                    self.hover_preview = new_hover;
                    ctx.request_repaint();
                }

                // Click commits the current cursor position as a real
                // waypoint and stamps the dirty flag so the debounce
                // timer schedules a match. `interact_pointer_pos`
                // returns the position at the moment of the click,
                // not the latest hover position — important if the
                // cursor moved slightly between mousedown and mouseup.
                if response.clicked() {
                    if let Some(pos) = response.interact_pointer_pos() {
                        let p = projector.unproject(egui::vec2(pos.x, pos.y));
                        self.drawn_path.push(Coord {
                            x: p.x(),
                            y: p.y(),
                        });
                        self.last_draw_change = Some(Instant::now());
                    }
                }
            } else if self.hover_preview.is_some() {
                // Tidy up when leaving draw mode.
                self.hover_preview = None;
            }
        });

        // Debounced match against the drawn path. Runs at most once per
        // `DRAW_DEBOUNCE` window of inactivity; that's enough to coalesce
        // a quick burst of clicks into a single solve while staying
        // responsive when the user pauses.
        if let Some(ts) = self.last_draw_change {
            if ts.elapsed() >= DRAW_DEBOUNCE && self.drawn_path.len() >= 2 {
                let line: LineString = LineString::new(self.drawn_path.clone());
                self.match_linestring(line, /* recenter = */ false);
                self.last_draw_change = None;
            } else {
                // Keep the frame loop ticking so the debounce timer
                // resolves even without other input.
                ctx.request_repaint_after(DRAW_DEBOUNCE);
            }
        }
    }
}

/// Renders the user's in-flight drawn path plus a faded "where would my
/// next click land" preview from the last waypoint to the cursor. Owned
/// by the wrapper App because plugins can't mutate viewer state from
/// inside their `run`; the source of truth is `ViewerApp::drawn_path` /
/// `ViewerApp::hover_preview`, and we just clone it in each frame.
struct DrawPlugin {
    path: Vec<Coord>,
    hover: Option<Coord>,
}

impl Plugin for DrawPlugin {
    fn run(
        self: Box<Self>,
        ui: &mut egui::Ui,
        _response: &egui::Response,
        projector: &Projector,
        _map_memory: &MapMemory,
    ) {
        let painter = ui.painter();

        // Yellow committed path.
        if self.path.len() >= 2 {
            let pts: Vec<_> = self
                .path
                .iter()
                .map(|c| projector.project(lon_lat(c.x, c.y)).to_pos2())
                .collect();
            painter.line(pts, Stroke::new(3.0, Color32::from_rgb(255, 200, 0)));
        }

        // Faded preview from the last waypoint to the current cursor.
        if let (Some(last), Some(hover)) = (self.path.last(), self.hover) {
            let a = projector.project(lon_lat(last.x, last.y)).to_pos2();
            let b = projector.project(lon_lat(hover.x, hover.y)).to_pos2();
            painter.line(
                vec![a, b],
                Stroke::new(
                    2.0,
                    Color32::from_rgba_unmultiplied_const(255, 200, 0, 90),
                ),
            );
        }

        // Waypoint dots — small filled circle with a black ring so they
        // remain visible against any tile colour.
        for c in &self.path {
            let pos = projector.project(lon_lat(c.x, c.y)).to_pos2();
            painter.circle_filled(pos, 5.0, Color32::from_rgb(255, 200, 0));
            painter.circle_stroke(pos, 5.0, Stroke::new(1.0, Color32::BLACK));
        }
    }
}

struct LinePlugin {
    original: LineString,

    interpolated: LineString,
    discrete: LineString,
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

            painter.line(
                pts,
                Stroke::new(4.0, Color32::from_rgba_unmultiplied_const(0, 100, 255, 128)),
            );
        }

        // Draw original WKT as RED (faded)
        if !self.original.0.is_empty() {
            let pts: Vec<_> = self
                .original
                .0
                .iter()
                .map(|p| projector.project(lon_lat(p.x, p.y)).to_pos2())
                .collect();

            painter.line(pts, Stroke::new(2.0, Color32::from_rgb(255, 0, 0)));
        }

        for (a, b) in self.original.into_iter().zip(self.discrete.into_iter()) {
            let p_original = projector.project(lon_lat(a.x, a.y)).to_pos2();
            let p_matched = projector.project(lon_lat(b.x, b.y)).to_pos2();

            let pos = projector.project(lon_lat(a.x, a.y)).to_pos2();

            painter.circle_filled(pos, 4.0, Color32::RED);
            painter.circle_stroke(pos, 4.0, Stroke::new(1.0, Color32::BLACK));

            let pos = projector.project(lon_lat(b.x, b.y)).to_pos2();

            painter.circle_filled(pos, 4.0, Color32::BLUE);
            painter.circle_stroke(pos, 4.0, Stroke::new(1.0, Color32::BLACK));

            painter.line(
                vec![p_original, p_matched],
                Stroke::new(2.0, Color32::from_rgb(50, 50, 50)),
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

struct CandidatePlugin<E: Entry> {
    candidates: Vec<(CandidateId, Candidate<E>)>,
    chosen_id: Option<CandidateId>,
    selected_id: Option<CandidateId>,
    original_coord: Option<Coord>,
    _ph: PhantomData<E>,
}

impl<E: Entry> Plugin for CandidatePlugin<E> {
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
