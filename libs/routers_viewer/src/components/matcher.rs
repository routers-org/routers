use std::sync::Arc;

use egui::Response;
use geo::{Coord, LineString};
use routers::PredicateCache;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork, OsmTripConfiguration};
use routers_transition::{
    MatchState, Solver, costing::CostingStrategies, entity::Transition,
    layer::generation::StandardGenerator, solver::all_compute::AllComputeSolver,
};

use crate::utils::{Component, MatchCandidate, MatchData, MatchLayer};

pub type MatchCache = Arc<PredicateCache<OsmEntryId, OsmEdgeMetadata, OsmNetwork>>;

pub struct MatchOutput {
    pub confirmed: Option<Result<MatchData, String>>,
    pub preview: Option<MatchData>,
}

pub struct Matcher<'a> {
    network: &'a OsmNetwork,
    cache: MatchCache,
    input: Option<LineString>,
    drawn_points: Vec<Coord>,
    cursor: Option<Coord>,
}

impl<'a> Matcher<'a> {
    pub fn new(network: &'a OsmNetwork, input: Option<LineString>, cache: MatchCache) -> Self {
        Self {
            network,
            input,
            cache,
            drawn_points: vec![],
            cursor: None,
        }
    }

    pub fn drawn(mut self, pts: Vec<Coord>, cursor: Option<Coord>) -> Self {
        self.drawn_points = pts;
        self.cursor = cursor;
        self
    }
}

impl<'a> Component for Matcher<'a> {
    type Output = MatchOutput;

    fn draw(&self, _ctx: &crate::utils::Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let btn = ui.button("Match!");

        let confirmed = btn
            .clicked()
            .then(|| self.input.clone())
            .flatten()
            .map(|ls| run_match(self.network, ls, self.cache.clone()));

        let preview = self.cursor.and_then(|cursor| {
            if self.drawn_points.is_empty() {
                return None;
            }
            let mut pts = self.drawn_points.clone();
            pts.push(cursor);
            run_match(self.network, LineString::new(pts), self.cache.clone()).ok()
        });

        (btn, MatchOutput { confirmed, preview })
    }
}

fn run_match(
    network: &OsmNetwork,
    linestring: LineString,
    cache: MatchCache,
) -> Result<MatchData, String> {
    let costing = CostingStrategies::default();
    let generator = StandardGenerator::new(network, &costing.emission, 100.0);
    let transition = Transition::new(network, linestring.clone(), &costing, generator);
    let solver = AllComputeSolver::default().use_cache(cache);
    let runtime = OsmTripConfiguration::default();
    let mut state = MatchState::default();

    let start = std::time::Instant::now();
    let collapsed = solver
        .solve(transition, &runtime, &mut state)
        .map_err(|e| format!("{e:?}"))?;
    let time = start.elapsed();

    let interpolated_line = collapsed.interpolated(network);

    // candidates.lookup is scc::HashMap — exposes .scan(), not std Iterator.
    let route = collapsed.route.clone();

    let mut max_layer: Option<usize> = None;
    collapsed.candidates.lookup.scan(|_, cand| {
        let l = cand.location.layer_id;
        max_layer = Some(max_layer.map_or(l, |m| m.max(l)));
    });

    let num_layers = max_layer.map(|m| m + 1).unwrap_or(0);
    let mut layers: Vec<MatchLayer> = (0..num_layers)
        .map(|_| MatchLayer {
            original: geo::Coord::default(),
            candidates: Vec::new(),
            chosen_idx: None,
        })
        .collect();

    collapsed.candidates.lookup.scan(|id, cand| {
        let layer_idx = cand.location.layer_id;
        let layer = &mut layers[layer_idx];
        let is_chosen = route.get(layer_idx) == Some(id);
        layer.candidates.push(MatchCandidate {
            position: cand.position.0,
            emission: cand.emission,
        });
        if is_chosen {
            layer.chosen_idx = Some(layer.candidates.len() - 1);
        }
    });

    for (i, pt) in linestring.0.iter().enumerate() {
        if let Some(layer) = layers.get_mut(i) {
            layer.original = *pt;
        }
    }

    for layer in &mut layers {
        let chosen_pos = layer.chosen_idx.map(|i| layer.candidates[i].position);
        layer.candidates.sort_by_key(|c| c.emission);
        layer.chosen_idx =
            chosen_pos.and_then(|pos| layer.candidates.iter().position(|c| c.position == pos));
    }

    Ok(MatchData {
        cost: collapsed.cost,
        time,
        original_line: linestring,
        interpolated_line,
        layers,
    })
}
