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
        let (start, end) = {
            // Compute cost ~= free
            transition
                .candidates
                .attach_ends(&transition.layers)
                .map_err(MatchError::EndAttachFailure)?
        };

        debug!("Attached Ends");
        transition.candidates.weave(&transition.layers);
        debug!("Weaved all candidate layers.");

        info!("Solving: Start={start:?}. End={end:?}. ");
        let context = transition.context(runtime);

        let t_solve_start = Instant::now();
        let n_layers = transition.layers.layers.len();
        let mut per_layer_ms: Vec<f64> = Vec::new();

        // Pre-generate KV pair
        let t_gen = Instant::now();
        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> = if solve_profile_per_layer() {
            // Serial outer loop for per-layer attribution; inner stays parallel.
            let mut acc: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> = FxHashMap::default();
            for layer in &transition.layers.layers {
                let t_layer = Instant::now();
                let layer_entries: Vec<(CandidateId, Vec<(CandidateId, CandidateEdge)>)> = layer
                    .nodes
                    .par_iter()
                    .map(|source| {
                        let found = self.reach(&transition, &context, source);
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
                        let found = self.reach(&transition, &context, source);
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

        if let Some(all) = transition.layers.layers.last() {
            for node in &all.nodes {
                pair.insert(*node, vec![(end, CandidateEdge::zero())]);
            }
        }

        // Note: For every candidate, generate their reachable elements, then run the solver overtop.
        //       This means we can do it in parallel, which is more efficient - however will have to
        //       compute for *every* candidate, not just the likely ones, which will lead to poor
        //       scalability for really long-routes.
        //
        //       This behaviour can be implemented using the `AllForwardSolver` going forward.

        let t_astar = Instant::now();
        let Some((path, cost)) = ({
            debug_time!("Solved transition graph");

            astar(
                &start,
                |source| pair.get(source).cloned().unwrap_or(vec![]),
                |_| CandidateEdge::zero(),
                |node| *node == end,
            )
        }) else {
            return Err(MatchError::CollapseFailure(CollapseError::NoPathFound));
        };
        let astar_ms = t_astar.elapsed().as_secs_f64() * 1000.0;

        // Phase-0 profiling: sampled per-stage timing log.
        // `gen_ms` is the time the warm step would skip (all reach work for
        // layer pairs 0..n-2). `astar_ms` is the part it still pays.
        let sample_n = solve_profile_sample_n();
        if sample_n > 0 {
            let n = SOLVE_PROFILE_COUNTER.fetch_add(1, Ordering::Relaxed);
            if n % sample_n == 0 {
                let solve_ms = t_solve_start.elapsed().as_secs_f64() * 1000.0;
                let gen_pct = if solve_ms > 0.0 {
                    (gen_ms / solve_ms) * 100.0
                } else {
                    0.0
                };
                let astar_pct = if solve_ms > 0.0 {
                    (astar_ms / solve_ms) * 100.0
                } else {
                    0.0
                };
                let per_layer_str = if per_layer_ms.is_empty() {
                    String::new()
                } else {
                    let parts: Vec<String> = per_layer_ms
                        .iter()
                        .enumerate()
                        .map(|(i, ms)| format!("L{i}={ms:.2}"))
                        .collect();
                    format!(" per_layer=[{}]", parts.join(","))
                };
                info!(
                    target: "solver_profile",
                    "solve_ms={solve_ms:.2} gen_ms={gen_ms:.2} ({gen_pct:.1}%) astar_ms={astar_ms:.2} ({astar_pct:.1}%) layers={n_layers}{per_layer_str}"
                );
            }
        }

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

        // Update candidate graph with calculated weights
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

        Ok(CollapsedPath::new(
            cost.weight,
            reached,
            path,
            transition.candidates,
            #[cfg(debug_assertions)]
            considered,
        ))
    }
}
