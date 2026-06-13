//! Streaming matcher: extends a saved Viterbi frontier by one event.

use crate::candidate::CollapsedPath;
use crate::costing::{CostingStrategies, EmissionContext, EmissionStrategy, TransitionStrategy};
use crate::generation::StandardGenerator;
use crate::primitives::MatchError;
use crate::solver::PrecomputeForwardSolver;
use crate::transition::candidate::{
    Candidate, CandidateId, CandidateLocation, CandidateRef, Candidates, OpenCandidateGraph,
    RoutedPath,
};
use crate::transition::entity::Transition;
use crate::transition::layer::definition::{Layer, Layers};
use crate::transition::streaming::state::{FrontierNode, MatchState};
use geo::{Distance, Haversine, LineString, Point};
use routers_network::{Edge, Entry, Metadata, Network};

/// Holds the immutable per-pod configuration the streaming step needs
/// (network, heuristics, search distance) and exposes operations that
/// extend a saved Viterbi frontier by one new GPS observation.
pub struct StreamingMatcher<'a, E, M, N, Emmis, Trans>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E, M, N> + Send + Sync,
{
    map: &'a N,
    heuristics: &'a CostingStrategies<Emmis, Trans, E, M, N>,
    search_distance: f64,
}

/// One candidate's data prior to placement in a candidate graph.
/// Holds everything `Candidate::new` needs except `location`, which is
/// assigned by [`StreamingMatcher::assemble`] based on layer position.
struct CandidateData<E: Entry> {
    edge: Edge<E>,
    position: Point,
    emission: u32,
}

/// A layer's worth of candidates plus the GPS origin used during
/// transition costing.
struct LayerCandidates<E: Entry> {
    origin: Point,
    candidates: Vec<CandidateData<E>>,
}

impl<E: Entry> LayerCandidates<E> {
    /// Derive a layer from a saved Viterbi frontier. Each frontier
    /// node becomes a candidate whose `emission` is the prior
    /// `cum_cost`, so the seeded start → L0 edges built by the solver
    /// carry the accumulated cost forward.
    fn from_frontier(frontier: &[FrontierNode<E>]) -> Self {
        let origin = frontier
            .first()
            .map(|f| f.snapped)
            .unwrap_or_else(|| Point::new(0.0, 0.0));
        let candidates = frontier
            .iter()
            .map(|f| CandidateData {
                edge: f.edge,
                position: f.snapped,
                emission: f.cum_cost,
            })
            .collect();
        Self { origin, candidates }
    }

    fn len(&self) -> usize {
        self.candidates.len()
    }
}

impl<'a, E, M, N, Emmis, Trans> StreamingMatcher<'a, E, M, N, Emmis, Trans>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E, M, N> + Send + Sync,
{
    /// Construct a matcher bound to a network and costing.
    pub fn new(
        map: &'a N,
        heuristics: &'a CostingStrategies<Emmis, Trans, E, M, N>,
        search_distance: f64,
    ) -> Self {
        Self {
            map,
            heuristics,
            search_distance,
        }
    }

    /// Extend a saved `MatchState` by one new GPS observation.
    ///
    /// Returns the matched path (for emitting `MatchResult` /
    /// `MatchRoute`) and the new state ready for write-back.
    ///
    /// `prev` must be non-empty; callers should cold-start when
    /// [`MatchState::is_empty`] is `true`.
    pub fn step(
        &self,
        prev: &MatchState<E>,
        new_point: Point,
        event_ms: u64,
        runtime: &M::Runtime,
    ) -> Result<(RoutedPath<E, M>, MatchState<E>), MatchError> {
        let l0 = LayerCandidates::from_frontier(&prev.frontier);
        let l1 = self.observe_at(new_point);
        let transition = self.assemble(vec![l0, l1]);

        let solver = PrecomputeForwardSolver::<E, M, N>::default();
        let (collapsed, l_last) = solver.solve_with_frontier(transition, runtime)?;
        Ok(self.finalize(collapsed, l_last, event_ms))
    }

    /// Run a full HMM solve over a multi-point linestring and return
    /// the matched path plus the L_last column. Use this when no
    /// prior state exists for a vehicle (first-ever event, or after
    /// state eviction / cost-ceiling re-anchor). The returned state
    /// can be fed back into [`Self::step`] on the next event.
    pub fn cold_start(
        &self,
        linestring: LineString,
        event_ms: u64,
        runtime: &M::Runtime,
    ) -> Result<(RoutedPath<E, M>, MatchState<E>), MatchError> {
        let generator =
            StandardGenerator::new(self.map, &self.heuristics.emission, self.search_distance);
        let transition = Transition::new(self.map, linestring, self.heuristics, generator);

        let solver = PrecomputeForwardSolver::<E, M, N>::default();
        let (collapsed, l_last) = solver.solve_with_frontier(transition, runtime)?;
        Ok(self.finalize(collapsed, l_last, event_ms))
    }

    /// Bundle a solver output into the public return shape: a
    /// [`RoutedPath`] for emitting downstream messages and a
    /// [`MatchState`] ready for write-back into the per-vehicle cache.
    fn finalize(
        &self,
        collapsed: CollapsedPath<E>,
        l_last: Vec<(CandidateId, u32)>,
        event_ms: u64,
    ) -> (RoutedPath<E, M>, MatchState<E>) {
        let frontier: Vec<FrontierNode<E>> = l_last
            .into_iter()
            .filter_map(|(id, cum_cost)| {
                let c = collapsed.candidates.candidate(&id)?;
                Some(FrontierNode {
                    edge: c.edge,
                    snapped: c.position,
                    cum_cost,
                })
            })
            .collect();

        (
            RoutedPath::new(collapsed, self.map),
            MatchState::new(frontier, event_ms),
        )
    }

    /// Project a GPS point onto every road within the search radius
    /// and return the resulting layer's candidate data.
    fn observe_at(&self, point: Point) -> LayerCandidates<E> {
        let candidates = self
            .map
            .nearest_nodes_projected(&point, self.search_distance)
            .map(|(position, edge)| {
                let distance = Haversine.distance(position, point);
                let emission = self.heuristics.emission.cost(EmissionContext::new(
                    &position,
                    &point,
                    distance,
                    edge.weight,
                ));
                CandidateData {
                    edge: edge.thin(),
                    position,
                    emission,
                }
            })
            .collect();
        LayerCandidates {
            origin: point,
            candidates,
        }
    }

    /// Combine layer specifications into a `Transition`. Assigns each
    /// candidate's `location.layer_id` and `node_id` from its position
    /// in the input.
    fn assemble(&self, layers: Vec<LayerCandidates<E>>) -> Transition<'a, Emmis, Trans, E, M, N> {
        let total: usize = layers.iter().map(LayerCandidates::len).sum();
        let mut graph = OpenCandidateGraph::with_capacity(total, 0);
        let lookup = scc::HashMap::with_capacity(total);

        let final_layers: Vec<Layer> = layers
            .into_iter()
            .enumerate()
            .map(|(layer_id, layer)| {
                let mut nodes = Vec::with_capacity(layer.candidates.len());
                for (node_id, data) in layer.candidates.into_iter().enumerate() {
                    let candidate = Candidate::new(
                        data.edge,
                        data.position,
                        data.emission,
                        CandidateLocation { layer_id, node_id },
                    );
                    let candidate_ref = CandidateRef::new(data.emission);
                    let id = graph.add_node(candidate_ref);
                    let _ = lookup.insert(id, candidate);
                    nodes.push(id);
                }
                Layer {
                    nodes,
                    origin: layer.origin,
                }
            })
            .collect();

        Transition::from_parts(
            self.map,
            self.heuristics,
            Layers { layers: final_layers },
            Candidates::new(graph, lookup),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::r#match::DEFAULT_SEARCH_DISTANCE;
    use crate::testing::{MockEntryId, MockMetadata, MockNetwork, MockNetworkBuilder};
    use geo::point;
    use routers_network::Discovery;

    fn straight_road() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1, point!(x: -118.14, y: 34.15))
            .node(2, point!(x: -118.15, y: 34.15))
            .node(3, point!(x: -118.16, y: 34.15))
            .node(4, point!(x: -118.17, y: 34.15))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .build()
    }

    fn one_node_frontier(net: &MockNetwork) -> (Vec<FrontierNode<MockEntryId>>, Point) {
        let edge = net
            .edge(&MockEntryId(1), &MockEntryId(2))
            .expect("edge 1→2 exists in straight_road");
        let frontier = vec![FrontierNode {
            edge,
            snapped: point!(x: -118.145, y: 34.15),
            cum_cost: 42,
        }];
        let new_point = point!(x: -118.155, y: 34.15);
        (frontier, new_point)
    }

    #[test]
    fn from_frontier_one_candidate_per_node() {
        let frontier = vec![
            FrontierNode {
                edge: dummy_edge(),
                snapped: point!(x: 0.0, y: 0.0),
                cum_cost: 1,
            },
            FrontierNode {
                edge: dummy_edge(),
                snapped: point!(x: 1.0, y: 0.0),
                cum_cost: 2,
            },
        ];
        let layer = LayerCandidates::from_frontier(&frontier);
        assert_eq!(layer.len(), 2);
    }

    #[test]
    fn from_frontier_preserves_cum_cost_as_emission() {
        let frontier = vec![FrontierNode {
            edge: dummy_edge(),
            snapped: point!(x: 0.0, y: 0.0),
            cum_cost: 99,
        }];
        let layer = LayerCandidates::from_frontier(&frontier);
        assert_eq!(layer.candidates[0].emission, 99);
    }

    #[test]
    fn from_frontier_origin_is_first_snapped() {
        let first = point!(x: 1.0, y: 2.0);
        let frontier = vec![FrontierNode {
            edge: dummy_edge(),
            snapped: first,
            cum_cost: 0,
        }];
        let layer = LayerCandidates::from_frontier(&frontier);
        assert_eq!(layer.origin, first);
    }

    #[test]
    fn from_frontier_empty_input_yields_empty_layer() {
        let empty: [FrontierNode<MockEntryId>; 0] = [];
        let layer = LayerCandidates::from_frontier(&empty);
        assert_eq!(layer.len(), 0);
    }

    #[test]
    fn observe_at_yields_candidates_near_point() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let layer = matcher.observe_at(point!(x: -118.155, y: 34.1503));
        assert!(layer.len() > 0, "near a road, expect ≥1 candidate");
    }

    #[test]
    fn observe_at_origin_is_input_point() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let p = point!(x: -118.155, y: 34.1503);
        let layer = matcher.observe_at(p);
        assert_eq!(layer.origin, p);
    }

    #[test]
    fn assemble_produces_one_layer_per_input() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let (frontier, p) = one_node_frontier(&net);

        let transition = matcher.assemble(vec![
            LayerCandidates::from_frontier(&frontier),
            matcher.observe_at(p),
        ]);
        assert_eq!(transition.layers.layers.len(), 2);
    }

    #[test]
    fn assemble_assigns_layer_ids_by_position() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let (frontier, p) = one_node_frontier(&net);

        let transition = matcher.assemble(vec![
            LayerCandidates::from_frontier(&frontier),
            matcher.observe_at(p),
        ]);

        for (expected_layer_id, layer) in transition.layers.layers.iter().enumerate() {
            for id in &layer.nodes {
                let c = transition
                    .candidates
                    .candidate(id)
                    .expect("candidate present");
                assert_eq!(c.location.layer_id, expected_layer_id);
            }
        }
    }

    #[test]
    fn step_returns_non_empty_state_and_path() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let (frontier, new_point) = one_node_frontier(&net);
        let prev = MatchState::new(frontier, 100);

        let (routed, new_state) = matcher
            .step(&prev, new_point, 200, &())
            .expect("step must succeed on a reachable frontier");

        assert!(!new_state.is_empty());
        assert_eq!(new_state.last_event_ms, 200);
        assert!(!routed.discretized.elements.is_empty());
    }

    #[test]
    fn step_argmin_matches_path_terminal() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let (frontier, new_point) = one_node_frontier(&net);
        let prev = MatchState::new(frontier, 100);

        let (routed, new_state) = matcher.step(&prev, new_point, 200, &()).expect("step");
        let path_terminal = routed.discretized.elements.last().expect("terminal");
        let argmin = new_state.argmin().expect("argmin");
        assert_eq!(argmin.snapped, geo::Point::from(path_terminal.point));
    }

    #[test]
    fn cold_start_seeds_frontier_for_next_step() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let linestring: LineString = geo::wkt! {
            LINESTRING(
                -118.141 34.1503,
                -118.155 34.1503
            )
        };

        let (routed, state) = matcher
            .cold_start(linestring, 100, &())
            .expect("cold-start solves a reachable linestring");

        assert!(!state.is_empty(), "cold-start populates frontier for subsequent step");
        assert_eq!(state.last_event_ms, 100);
        assert!(!routed.discretized.elements.is_empty());

        // The state must be usable as input to step().
        let next_point = geo::point!(x: -118.165, y: 34.1503);
        let (_, _) = matcher
            .step(&state, next_point, 200, &())
            .expect("cold-start output feeds back into step cleanly");
    }

    #[test]
    fn step_advances_last_event_ms() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);
        let (frontier, new_point) = one_node_frontier(&net);
        let prev = MatchState::new(frontier, 100);

        let (_routed, new_state) = matcher.step(&prev, new_point, 250, &()).expect("step");
        assert_eq!(new_state.last_event_ms, 250);
    }

    /// Even spacing across `straight_road`'s 4 nodes
    /// (-118.14 .. -118.17, y=34.1503). Returns `n` reachable points.
    fn equivalence_points(n: usize) -> Vec<Point> {
        let (start, end) = (-118.141_f64, -118.169_f64);
        let step = (end - start) / ((n - 1) as f64);
        (0..n)
            .map(|i| point!(x: start + (i as f64) * step, y: 34.1503))
            .collect()
    }

    /// Cold-start over a 2-point prefix, then warm-step the remainder
    /// one point at a time. Mirrors the matcher binary's streaming path.
    fn streaming_chain(
        matcher: &StreamingMatcher<
            '_,
            MockEntryId,
            MockMetadata,
            MockNetwork,
            crate::costing::DefaultEmissionCost,
            crate::costing::DefaultTransitionCost,
        >,
        points: &[Point],
    ) -> MatchState<MockEntryId> {
        let seed = LineString(vec![points[0].into(), points[1].into()]);
        let (_, mut state) = matcher.cold_start(seed, 100, &()).expect("seed cold-start");
        for (i, p) in points.iter().enumerate().skip(2) {
            let (_, ns) = matcher
                .step(&state, *p, 100 * (i as u64 + 1), &())
                .expect("warm step");
            state = ns;
        }
        state
    }

    /// Solve the full N-point linestring in one shot.
    fn full_solve(
        matcher: &StreamingMatcher<
            '_,
            MockEntryId,
            MockMetadata,
            MockNetwork,
            crate::costing::DefaultEmissionCost,
            crate::costing::DefaultTransitionCost,
        >,
        points: &[Point],
    ) -> MatchState<MockEntryId> {
        let ls = LineString(points.iter().map(|p| (*p).into()).collect());
        let (_, state) = matcher.cold_start(ls, 100, &()).expect("full cold-start");
        state
    }

    /// 1C-13: by the Markov property, a streaming N-step chain must
    /// agree with the equivalent full N-layer solve at every length.
    /// We assert argmin equality (edge + snapped + cum_cost) — this
    /// is the load-bearing invariant for the downstream `MatchResult`.
    #[test]
    fn streaming_chain_matches_full_solve_argmin() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);

        for n in [3usize, 4, 5] {
            let points = equivalence_points(n);
            let full = full_solve(&matcher, &points);
            let stream = streaming_chain(&matcher, &points);

            let f = full.argmin().expect("full argmin present");
            let s = stream.argmin().expect("stream argmin present");
            assert_eq!(f.cum_cost, s.cum_cost, "n={n}: cum_cost mismatch");
            assert_eq!(f.snapped, s.snapped, "n={n}: snapped mismatch");
            assert_eq!(f.edge.id, s.edge.id, "n={n}: edge mismatch");
        }
    }

    /// 1C-14: focused warm-step argmin parity. A single warm extension
    /// of an (N-1)-point cold-start must produce the same argmin as
    /// the full N-point solve.
    #[test]
    fn warm_step_matches_full_solve_argmin() {
        let net = straight_road();
        let costing = CostingStrategies::default();
        let matcher = StreamingMatcher::new(&net, &costing, DEFAULT_SEARCH_DISTANCE);

        let points = equivalence_points(4);

        let full_ls = LineString(points.iter().map(|p| (*p).into()).collect());
        let (_, full_state) = matcher.cold_start(full_ls, 100, &()).expect("full");

        let prefix_ls =
            LineString(points[..3].iter().map(|p| (*p).into()).collect());
        let (_, prev) = matcher.cold_start(prefix_ls, 100, &()).expect("prefix");
        let (_, warm_state) =
            matcher.step(&prev, points[3], 200, &()).expect("warm step");

        let f = full_state.argmin().expect("full argmin");
        let w = warm_state.argmin().expect("warm argmin");
        assert_eq!(f.cum_cost, w.cum_cost, "cum_cost mismatch");
        assert_eq!(f.snapped, w.snapped, "snapped mismatch");
        assert_eq!(f.edge.id, w.edge.id, "edge mismatch");
    }

    fn dummy_edge() -> Edge<MockEntryId> {
        use routers_network::DirectionAwareEdgeId;
        Edge {
            source: MockEntryId(0),
            target: MockEntryId(0),
            weight: 0,
            id: DirectionAwareEdgeId::new(MockEntryId(0)),
        }
    }
}
