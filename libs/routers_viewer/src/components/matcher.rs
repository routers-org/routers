use std::sync::Arc;

use egui::Response;
use geo::{Coord, LineString};
use routers::{
    LayerId, layer::generation::StandardGenerator, primitives::PredicateCache, weigh::Selective,
};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork, OsmTripConfiguration};
use routers_transition::{Matcher as TransitionMatcher, costing::CostingStrategies};

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
    let generator = StandardGenerator::new(network, &costing.emission).with_search_distance(100.0);
    let weigher = Selective::default().use_cache(cache);
    let runtime = OsmTripConfiguration::default();
    let matcher = TransitionMatcher::new(network, &costing, generator, weigher, &runtime);

    let start = std::time::Instant::now();
    let collapsed = matcher
        .r#match(linestring.clone())
        .map_err(|e| format!("{e:?}"))?;
    let time = start.elapsed();

    let interpolated_line = collapsed.interpolated(network);
    let route = &collapsed.route;

    let mut layers: Vec<MatchLayer> = (0..collapsed.candidates.layers())
        .map(|l| {
            let candidates = collapsed
                .candidates
                .layer(LayerId(l as u32))
                .unwrap_or_default();

            MatchLayer {
                original: linestring.0.get(l).copied().unwrap_or_default(),
                candidates: candidates
                    .iter()
                    .map(|cand| MatchCandidate {
                        position: cand.position.0,
                        emission: cand.emission,
                    })
                    .collect(),
                chosen_idx: route.get(l).map(|chosen| chosen.node.index()),
            }
        })
        .collect();

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
