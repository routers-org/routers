use egui::Response;
use geo::LineString;
use routers::transition::{
    costing::CostingStrategies,
    entity::Transition,
    layer::generation::StandardGenerator,
    solver::selective_forward::SelectiveForwardSolver,
};
use routers_codec::osm::{OsmNetwork, OsmTripConfiguration};
use routers_network::DataPlane;

use crate::{
    match_data::{CandidateViz, LayerViz, MatchData},
    utils::Component,
};

/// `None`       — button not clicked (or no valid input LineString).
/// `Some(Ok(_))` — match succeeded; caller should update the cache.
/// `Some(Err(_))` — match failed; caller should display the message.
pub type MatchResult = Option<Result<MatchData, String>>;

pub struct Matcher<'a> {
    network: &'a OsmNetwork,
    input: Option<LineString>,
}

impl<'a> Matcher<'a> {
    pub fn new(network: &'a OsmNetwork, input: Option<LineString>) -> Self {
        Self { network, input }
    }
}

impl<'a> Component for Matcher<'a> {
    type Output = MatchResult;

    fn draw(&self, _ctx: &crate::utils::Context, ui: &mut egui::Ui) -> (Response, Self::Output) {
        let btn = ui.button("Match!");

        if let Some(linestring) = &self.input
            && btn.clicked()
        {
            let result = run_match(self.network, linestring.clone());
            return (btn, Some(result));
        }

        (btn, None)
    }
}

fn run_match(network: &OsmNetwork, linestring: LineString) -> Result<MatchData, String> {
    let costing = CostingStrategies::default();
    let generator = StandardGenerator::new(network, &costing.emission, 100.0);
    let transition = Transition::new(network, linestring.clone(), &costing, generator);
    let solver = SelectiveForwardSolver::default();
    let runtime = OsmTripConfiguration::default();

    let collapsed = transition
        .solve(solver, &runtime)
        .map_err(|e| format!("{e:?}"))?;

    // Pre-compute the full interpolated road geometry while we still have the
    // network reference (plugins are 'static, so they can't borrow it later).
    let interpolated_line = collapsed.interpolated(network);

    // candidates.lookup is scc::hash_map::HashMap — use .scan() not .iter().
    // Clone route first to keep it independent of the scan's borrow.
    let route = collapsed.route.clone();

    // First pass: determine the number of layers.
    let mut max_layer: Option<usize> = None;
    collapsed.candidates.lookup.scan(|_, cand| {
        let l = cand.location.layer_id;
        max_layer = Some(max_layer.map_or(l, |m| m.max(l)));
    });

    let num_layers = max_layer.map(|m| m + 1).unwrap_or(0);
    let mut layers: Vec<LayerViz> = (0..num_layers)
        .map(|_| LayerViz {
            original: geo::Coord::default(),
            candidates: Vec::new(),
            chosen_idx: None,
        })
        .collect();

    // Second pass: populate layers from each candidate.
    collapsed.candidates.lookup.scan(|id, cand| {
        let layer_idx = cand.location.layer_id;
        let layer = &mut layers[layer_idx];
        let is_chosen = route.get(layer_idx) == Some(id);
        layer.candidates.push(CandidateViz {
            position: cand.position.0,
            emission: cand.emission,
        });
        if is_chosen {
            layer.chosen_idx = Some(layer.candidates.len() - 1);
        }
    });

    // Attach original GPS coords to each layer.
    for (i, pt) in linestring.0.iter().enumerate() {
        if let Some(layer) = layers.get_mut(i) {
            layer.original = *pt;
        }
    }

    // Pre-compute the road-network node sequence for each chosen transition
    // between consecutive layers. Uses path_nodes() to deduplicate edges.
    let transitions: Vec<Vec<geo::Coord>> = collapsed
        .interpolated
        .iter()
        .map(|reachable| {
            reachable
                .path_nodes()
                .filter_map(|node_id| network.point(&node_id))
                .map(|pt| pt.0)
                .collect()
        })
        .collect();

    Ok(MatchData {
        cost: collapsed.cost,
        original_line: linestring,
        interpolated_line,
        layers,
        transitions,
    })
}
