use std::{
    marker::PhantomData,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};

use crate::transition::*;

use log::{debug, info};

use rayon::prelude::*;
use rustc_hash::FxHashMap;

use geo::{Distance, Haversine};
use itertools::Itertools;
use measure_time::debug_time;
use pathfinding::num_traits::Zero;
use pathfinding::prelude::*;
use routers_network::{Entry, Metadata, Network};

/// Phase-0 profiling: log per-stage solve timings every Nth call.
/// Activated by setting `SOLVER_PROFILE_SAMPLE_N` env to a positive int
/// (e.g. `100` for one log line per 100 solves). Default 0 = off, no
/// overhead in the hot path beyond a single relaxed atomic increment.
static SOLVE_PROFILE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn solve_profile_sample_n() -> u64 {
    static CACHED: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| {
        std::env::var("SOLVER_PROFILE_SAMPLE_N")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    })
}

/// Phase-0 profiling: switch the outer layer iter from parallel to serial
/// so we can time each layer-pair's reach work separately. The *inner*
/// per-candidate iter stays parallel — that's where the speedup actually
/// lives (10 candidates/layer vs 5 layers, so inner parallelism dominates).
/// Off by default, no impact on prod code path.
fn solve_profile_per_layer() -> bool {
    static CACHED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *CACHED.get_or_init(|| std::env::var("SOLVER_PROFILE_PER_LAYER").is_ok())
}

/// A Upper-Bounded Dijkstra (UBD) algorithm.
///
/// TODO: Docs
pub struct PrecomputeForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    // Internally holds a successors cache
    predicate: Arc<PredicateCache<E, M, N>>,
    reachable_hash: scc::HashMap<(usize, usize), Reachable<E>>,

    _phantom: PhantomData<N>,
}

impl<E, M, N> Default for PrecomputeForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    fn default() -> Self {
        Self {
            predicate: Arc::new(PredicateCache::default()),
            reachable_hash: scc::HashMap::new(),
            _phantom: PhantomData,
        }
    }
}

impl<E, M, N> PrecomputeForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    pub fn use_cache(self, cache: Arc<PredicateCache<E, M, N>>) -> Self {
        Self {
            predicate: cache,
            ..self
        }
    }

    fn reach<'a, 'b, Emmis, Trans>(
        &'b self,
        transition: &'b Transition<'b, Emmis, Trans, E, M, N>,
        context: &'b RoutingContext<'b, E, M, N>,
        source: &CandidateId,
    ) -> Vec<(Reachable<E>, CandidateEdge)>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync,
        'b: 'a,
    {
        use rayon::prelude::{IntoParallelIterator, ParallelIterator};
        let layer = transition.candidates.next_layer(source);

        layer
            .into_par_iter()
            .filter_map(|target| self.get_reachable(context, source, &target))
            .filter_map(move |reachable| {
                let path_vec = reachable.path_nodes().collect_vec();
                let optimal_path = Trip::new_with_map(transition.map, &path_vec);

                let source = context.candidate(&reachable.source)?;
                let target = context.candidate(&reachable.target)?;

                let sl = transition.layers.layers.get(source.location.layer_id)?;
                let tl = transition.layers.layers.get(target.location.layer_id)?;
                let layer_width = Haversine.distance(sl.origin, tl.origin);

                let transition_cost = transition.heuristics.transition(TransitionContext {
                    map_path: &path_vec,
                    requested_resolution_method: reachable.resolution_method,

                    source_candidate: &reachable.source,
                    target_candidate: &reachable.target,
                    routing_context: context,

                    source_position: source.position,
                    target_position: target.position,

                    layer_width,
                    optimal_path,
                });

                let cost = target.emission.saturating_add(transition_cost);

                Some((reachable, CandidateEdge::new(cost)))
            })
            .collect::<Vec<_>>()
    }

    fn get_reachable<'a>(
        &self,
        ctx: &'a RoutingContext<'a, E, M, N>,
        source_id: &CandidateId,
        target_id: &CandidateId,
    ) -> Option<Reachable<E>> {
        let source = ctx.candidate(source_id)?;

        // Upper-Bounded reachable map containing a Child:Parent relation
        // Note: Parent is OsmEntryId::NULL, which will not be within the map,
        //       indicating the root element.
        let predicate_map = self.predicate.query(ctx, source.edge.target);

        // Get the candidate information of the target found
        let candidate = ctx.candidate(target_id)?;

        // Both candidates are on the same edge
        'stmt: {
            if candidate.edge.id.index() == source.edge.id.index() {
                let common_source = candidate.edge.source == source.edge.source;
                let common_target = candidate.edge.target == source.edge.target;

                let tracking_forward = common_source && common_target;

                let source_percentage = source.percentage(ctx.map)?;
                let target_percentage = candidate.percentage(ctx.map)?;

                return if tracking_forward && source_percentage <= target_percentage {
                    // We are moving forward, it is simply the distance between the nodes
                    Some(Reachable::new(*source_id, *target_id, vec![]).distance_only())
                } else {
                    // We are going "backwards", behaviour becomes dependent on
                    // the directionality of the edge. However, to return across the
                    // node is an independent transition, and is not covered.
                    break 'stmt;
                };
            }
        }

        // Generate the path to this target using the predicate map
        let path_to_target =
            Self::path_builder(&candidate.edge.source, &source.edge.target, &predicate_map)?;

        let path = path_to_target
            .windows(2)
            .filter_map(|pair| {
                if let [a, b] = pair {
                    return ctx.edge(a, b);
                }

                None
            })
            .collect::<Vec<_>>();

        Some(Reachable::new(*source_id, *target_id, path))
    }
}

/// Per-stage timings produced by `build_pair`.
pub(crate) struct PairBuildTimings {
    pub gen_ms: f64,
    pub per_layer_ms: Vec<f64>,
}

impl<E, M, N> PrecomputeForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    /// Attach synthetic start/end nodes and weave zero-cost
    /// placeholder edges between consecutive layers. Returns the
    /// `(start, end)` ids. Calling twice errors via
    /// `EndsAlreadyAttached`.
    pub(crate) fn prepare_trellis<Emmis, Trans>(
        transition: &mut Transition<Emmis, Trans, E, M, N>,
    ) -> Result<(CandidateId, CandidateId), MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync,
    {
        let (start, end) = transition
            .candidates
            .attach_ends(&transition.layers)
            .map_err(MatchError::EndAttachFailure)?;
        debug!("Attached Ends");
        transition.candidates.weave(&transition.layers);
        debug!("Weaved all candidate layers.");
        Ok((start, end))
    }

    /// Build the forward transition-graph DAG. For every candidate,
    /// computes its reachable next-layer candidates with
    /// `(transition + emission)` edge cost. Also inserts the
    /// synthetic start → L0 (emission-only) and L_last → end
    /// (zero-cost) edges that A* needs as anchors.
    pub(crate) fn build_pair<'a, Emmis, Trans>(
        &self,
        transition: &'a Transition<'a, Emmis, Trans, E, M, N>,
        context: &RoutingContext<'a, E, M, N>,
        start: CandidateId,
        end: CandidateId,
    ) -> (
        FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>>,
        PairBuildTimings,
    )
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync,
    {
        let mut per_layer_ms: Vec<f64> = Vec::new();
        let t_gen = Instant::now();

        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            if solve_profile_per_layer() {
                // Serial outer loop for per-layer attribution; inner stays parallel.
                let mut acc: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
                    FxHashMap::default();
                for layer in &transition.layers.layers {
                    let t_layer = Instant::now();
                    let layer_entries: Vec<(CandidateId, Vec<(CandidateId, CandidateEdge)>)> = layer
                        .nodes
                        .par_iter()
                        .map(|source| {
                            let found = self.reach(transition, context, source);
                            let some = found
                                .into_iter()
                                .map(|(mut reachable, edge)| {
                                    #[cfg(debug_assertions)]
                                    {
                                        reachable.cost = edge.weight;
                                    }
                                    self.reachable_hash
                                        .insert(reachable.hash(), reachable.clone())
                                        .expect("hash collision, must insert correctly.");
                                    (reachable.target, edge)
                                })
                                .collect::<Vec<_>>();
                            (*source, some)
                        })
                        .collect();
                    per_layer_ms.push(t_layer.elapsed().as_secs_f64() * 1000.0);
                    for (s, e) in layer_entries {
                        acc.insert(s, e);
                    }
                }
                acc
            } else {
                debug_time!("generate transition graph");
                transition
                    .layers
                    .layers
                    .par_iter()
                    .flat_map(|layer| {
                        layer.nodes.par_iter().map(|source| {
                            let found = self.reach(transition, context, source);
                            let some = found
                                .into_iter()
                                .map(|(mut reachable, edge)| {
                                    #[cfg(debug_assertions)]
                                    {
                                        reachable.cost = edge.weight;
                                    }
                                    self.reachable_hash
                                        .insert(reachable.hash(), reachable.clone())
                                        .expect("hash collision, must insert correctly.");
                                    (reachable.target, edge)
                                })
                                .collect::<Vec<_>>();
                            (*source, some)
                        })
                    })
                    .collect::<FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>>>()
            };
        let gen_ms = t_gen.elapsed().as_secs_f64() * 1000.0;

        // start → L0: edge cost equals each L0 candidate's emission,
        // which is how Viterbi seeds the recurrence.
        pair.insert(
            start,
            transition.layers.layers[0]
                .nodes
                .iter()
                .filter_map(|source| {
                    let c = context.candidate(source)?;
                    Some((*source, CandidateEdge::new(c.emission)))
                })
                .collect_vec(),
        );

        // L_last → end: zero-cost edges so A* converges on a single target.
        if let Some(all) = transition.layers.layers.last() {
            for node in &all.nodes {
                pair.insert(*node, vec![(end, CandidateEdge::zero())]);
            }
        }

        (pair, PairBuildTimings { gen_ms, per_layer_ms })
    }

    /// A* over the `pair` DAG from `start` to `end`. Returns the
    /// chosen path with its accumulated cost and elapsed search time.
    pub(crate) fn find_optimal_path(
        pair: &FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>>,
        start: CandidateId,
        end: CandidateId,
    ) -> Result<(Vec<CandidateId>, CandidateEdge, f64), MatchError> {
        let t_astar = Instant::now();
        let path_and_cost = {
            debug_time!("Solved transition graph");
            astar(
                &start,
                |source| pair.get(source).cloned().unwrap_or_default(),
                |_| CandidateEdge::zero(),
                |node| *node == end,
            )
        };
        let astar_ms = t_astar.elapsed().as_secs_f64() * 1000.0;
        let (path, cost) = path_and_cost
            .ok_or(MatchError::CollapseFailure(CollapseError::NoPathFound))?;
        Ok((path, cost, astar_ms))
    }

    /// Like [`Solver::solve`] but also returns the L_last Viterbi
    /// column with per-candidate cumulative path costs — the input to
    /// the streaming append step.
    ///
    /// The frontier is `Vec<(CandidateId, cum_cost)>` for every
    /// candidate in the last user-layer that is reachable from
    /// `start`. The argmin of this frontier matches the chosen path's
    /// terminal candidate (the one before the synthetic `end`).
    pub fn solve_with_frontier<Emmis, Trans>(
        &self,
        mut transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
    ) -> Result<(CollapsedPath<E>, Vec<(CandidateId, u32)>), MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync,
    {
        let (start, end) = Self::prepare_trellis(&mut transition)?;

        let pair = {
            let context = transition.context(runtime);
            self.build_pair(&transition, &context, start, end).0
        };

        let (path, cost, _astar_ms) = Self::find_optimal_path(&pair, start, end)?;

        let frontier = crate::transition::streaming::ViterbiFrontier::from_pair(
            &pair,
            start,
            &transition.layers,
        )
        .last_layer();

        let collapsed = self.materialize_collapsed_path(transition, path, cost);
        Ok((collapsed, frontier))
    }

    /// Build a `CollapsedPath` by reconstructing `Reachable` objects
    /// along the chosen path from `reachable_hash`. Debug builds also
    /// collect `considered` reachables for visualisation.
    pub(crate) fn materialize_collapsed_path<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M, N>,
        path: Vec<CandidateId>,
        cost: CandidateEdge,
    ) -> CollapsedPath<E>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync,
    {
        info!("Total cost of solve: {}", cost.weight);
        let reached = path
            .windows(2)
            .filter_map(|nodes| {
                if let [a, b] = nodes {
                    self.reachable_hash
                        .get(&(a.index(), b.index()))
                        .map(|entry| entry.get().clone())
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Update candidate graph with calculated weights (debug only).
        #[cfg(debug_assertions)]
        let mut transition = transition;
        #[cfg(debug_assertions)]
        self.reachable_hash
            .scan(|&(a_idx, b_idx), &Reachable { cost, .. }| {
                let a = CandidateId::new(a_idx);
                let b = CandidateId::new(b_idx);
                if let Some(edge_idx) = transition.candidates.graph.find_edge(a, b) {
                    if let Some(edge) = transition.candidates.graph.edge_weight_mut(edge_idx) {
                        edge.weight = cost;
                    }
                }
            });

        #[cfg(debug_assertions)]
        let mut considered = Vec::new();
        #[cfg(debug_assertions)]
        self.reachable_hash.scan(|_, v| considered.push(v.clone()));

        CollapsedPath::new(
            cost.weight,
            reached,
            path,
            transition.candidates,
            #[cfg(debug_assertions)]
            considered,
        )
    }
}

impl<E, M, N> Solver<E, M, N> for PrecomputeForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    fn solve<Emmis, Trans>(
        &self,
        mut transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync,
    {
        let (start, end) = Self::prepare_trellis(&mut transition)?;
        info!("Solving: Start={start:?}. End={end:?}.");

        let t_solve_start = Instant::now();
        let n_layers = transition.layers.layers.len();

        // Scope the borrow so `context` drops before `transition` is
        // consumed by `materialize_collapsed_path` below.
        let (pair, timings) = {
            let context = transition.context(runtime);
            self.build_pair(&transition, &context, start, end)
        };

        let (path, cost, astar_ms) = Self::find_optimal_path(&pair, start, end)?;

        let sample_n = solve_profile_sample_n();
        if sample_n > 0
            && SOLVE_PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed) % sample_n == 0
        {
            let solve_ms = t_solve_start.elapsed().as_secs_f64() * 1000.0;
            let gen_pct = if solve_ms > 0.0 { (timings.gen_ms / solve_ms) * 100.0 } else { 0.0 };
            let astar_pct = if solve_ms > 0.0 { (astar_ms / solve_ms) * 100.0 } else { 0.0 };
            let per_layer_str = if timings.per_layer_ms.is_empty() {
                String::new()
            } else {
                let parts: Vec<String> = timings
                    .per_layer_ms
                    .iter()
                    .enumerate()
                    .map(|(i, ms)| format!("L{i}={ms:.2}"))
                    .collect();
                format!(" per_layer=[{}]", parts.join(","))
            };
            info!(
                target: "solver_profile",
                "solve_ms={solve_ms:.2} gen_ms={gen_ms:.2} ({gen_pct:.1}%) astar_ms={astar_ms:.2} ({astar_pct:.1}%) layers={n_layers}{per_layer_str}",
                gen_ms = timings.gen_ms,
            );
        }

        Ok(self.materialize_collapsed_path(transition, path, cost))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generation::StandardGenerator;
    use crate::testing::{MockEntryId, MockMetadata, MockNetwork, MockNetworkBuilder};
    use crate::transition::{CostingStrategies, Transition};
    use crate::r#match::DEFAULT_SEARCH_DISTANCE;
    use geo::{point, LineString, wkt};
    use rustc_hash::FxHashMap;

    /// Concrete `find_optimal_path` invocation; the three generic
    /// params are inferred-uninferable for a free-floating call so we
    /// pin them to the in-crate test types here.
    fn find_path(
        pair: &FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>>,
        start: CandidateId,
        end: CandidateId,
    ) -> Result<(Vec<CandidateId>, CandidateEdge, f64), MatchError> {
        <PrecomputeForwardSolver<
            crate::testing::MockEntryId,
            crate::testing::MockMetadata,
            crate::testing::MockNetwork,
        >>::find_optimal_path(pair, start, end)
    }

    #[test]
    fn picks_min_cost_route() {
        //   start ─(10)→ c0 ─(5)→ c1 ─(0)→ end
        //         ─(20)→ c2 ─(2)→ c1
        // start→c0→c1→end = 15  (optimal)
        // start→c2→c1→end = 22
        let start = CandidateId::new(0);
        let c0 = CandidateId::new(1);
        let c1 = CandidateId::new(2);
        let c2 = CandidateId::new(3);
        let end = CandidateId::new(4);

        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        pair.insert(start, vec![(c0, CandidateEdge::new(10)), (c2, CandidateEdge::new(20))]);
        pair.insert(c0, vec![(c1, CandidateEdge::new(5))]);
        pair.insert(c2, vec![(c1, CandidateEdge::new(2))]);
        pair.insert(c1, vec![(end, CandidateEdge::zero())]);

        let (path, cost, _) = find_path(&pair, start, end).expect("path exists");
        assert_eq!(path, vec![start, c0, c1, end]);
        assert_eq!(cost.weight, 15);
    }

    #[test]
    fn no_path_returns_err() {
        let start = CandidateId::new(0);
        let end = CandidateId::new(99);
        let pair = FxHashMap::default();
        assert!(matches!(
            find_path(&pair, start, end),
            Err(MatchError::CollapseFailure(CollapseError::NoPathFound))
        ));
    }

    /// A four-node straight road for end-to-end solver tests.
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

    #[test]
    fn solve_with_frontier_returns_l_last_argmin_matching_chosen_path() {
        let net = straight_road();
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.141 34.1503,
                -118.155 34.1503,
                -118.169 34.1503
            )
        };
        let costing = CostingStrategies::default();
        let generator = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
        let transition = Transition::new(&net, linestring, &costing, generator);

        let solver = PrecomputeForwardSolver::<MockEntryId, MockMetadata, MockNetwork>::default();
        let (collapsed, frontier) = solver
            .solve_with_frontier(transition, &())
            .expect("solve must succeed on a reachable network");

        assert!(!frontier.is_empty(), "L_last frontier must be non-empty");
        let argmin = frontier
            .iter()
            .min_by_key(|(_, cum)| *cum)
            .map(|(id, _)| *id)
            .expect("frontier has at least one entry");

        // The chosen path runs `start → … → L_last_winner → end`.
        // L_last_winner sits at the second-to-last index.
        let path_len = collapsed.route.len();
        assert!(path_len >= 2, "chosen path includes start + L_last_winner + end");
        let chosen_l_last = collapsed.route[path_len - 2];
        assert_eq!(
            argmin, chosen_l_last,
            "argmin of frontier must equal the chosen path's L_last candidate"
        );
    }

    #[test]
    fn solve_with_frontier_agrees_with_solve_on_chosen_path() {
        let net = straight_road();
        let linestring: LineString = wkt! {
            LINESTRING(
                -118.141 34.1503,
                -118.155 34.1503,
                -118.169 34.1503
            )
        };
        let costing = CostingStrategies::default();

        // solve()
        let generator = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
        let transition_a = Transition::new(&net, linestring.clone(), &costing, generator);
        let solver_a = PrecomputeForwardSolver::<MockEntryId, MockMetadata, MockNetwork>::default();
        let collapsed_a = solver_a.solve(transition_a, &()).expect("solve");

        // solve_with_frontier()
        let generator = StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
        let transition_b = Transition::new(&net, linestring, &costing, generator);
        let solver_b = PrecomputeForwardSolver::<MockEntryId, MockMetadata, MockNetwork>::default();
        let (collapsed_b, _frontier) = solver_b
            .solve_with_frontier(transition_b, &())
            .expect("solve_with_frontier");

        assert_eq!(
            collapsed_a.cost, collapsed_b.cost,
            "solve and solve_with_frontier must produce the same total cost"
        );
        assert_eq!(
            collapsed_a.route, collapsed_b.route,
            "solve and solve_with_frontier must pick the same path"
        );
    }

    #[test]
    fn picks_globally_cheaper_route() {
        // Greedy-first-edge choice (cost 1) leads to total 101;
        // optimal goes via the costlier first edge (50) for total 51.
        let start = CandidateId::new(0);
        let a = CandidateId::new(1);
        let b = CandidateId::new(2);
        let end = CandidateId::new(3);

        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        pair.insert(start, vec![(a, CandidateEdge::new(1)), (b, CandidateEdge::new(50))]);
        pair.insert(a, vec![(end, CandidateEdge::new(100))]);
        pair.insert(b, vec![(end, CandidateEdge::new(1))]);

        let (path, cost, _) = find_path(&pair, start, end).expect("path exists");
        assert_eq!(path, vec![start, b, end]);
        assert_eq!(cost.weight, 51);
    }
}
