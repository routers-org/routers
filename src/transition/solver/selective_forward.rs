use crate::{
    CollapseError, CollapsedPath, Costing, MatchError, PredicateCache, Reachable, Solver,
    TransitionContext, Trip,
    candidate::CandidateId,
    costing::{EmissionStrategy, TransitionStrategy},
    entity::Transition,
    primitives::RoutingContext,
};
use routers_network::{Entry, Metadata, Network};

use log::{debug, info};

use rustc_hash::FxHashMap;
use std::{marker::PhantomData, sync::Arc};

use geo::{Distance, Haversine};
use itertools::Itertools;
use measure_time::debug_time;
use rayon::prelude::*;

pub struct SelectiveForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    predicate: Arc<PredicateCache<E, M, N>>,
    _phantom: PhantomData<N>,
}

impl<E, M, N> Default for SelectiveForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    fn default() -> Self {
        Self {
            predicate: Arc::new(PredicateCache::default()),
            _phantom: PhantomData,
        }
    }
}

impl<E, M, N> SelectiveForwardSolver<E, M, N>
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

    /// Derives which candidates are reachable by the source candidate.
    ///
    /// Provides a slice of target candidate IDs, `targets`. The solver
    /// will use these to procure all candidates which are reachable,
    /// and the path of routable entries ([`OsmEntryId`]) which are used
    /// to reach the target.
    fn reachable<'a>(
        &self,
        ctx: &'a RoutingContext<'a, E, M, N>,
        source: &CandidateId,
        targets: &'a [CandidateId],
    ) -> Option<Vec<Reachable<E>>> {
        let source_candidate = ctx.candidate(source)?;

        // Upper-Bounded reachable map containing a Child:Parent relation
        // Note: Parent is OsmEntryId::NULL, which will not be within the map,
        //       indicating the root element.
        let predicate_map = {
            debug_time!("query predicate for {source:?}");

            self.predicate.query(ctx, source_candidate.edge.target)
        };

        let reachable = {
            debug_time!("predicates {source:?} -> reachable");

            targets
                .iter()
                .filter_map(|target| {
                    // Get the candidate information of the target found
                    let candidate = ctx.candidate(target)?;

                    // Both candidates are on the same edge
                    'stmt: {
                        if candidate.edge.id.index() == source_candidate.edge.id.index() {
                            let common_source =
                                candidate.edge.source == source_candidate.edge.source;
                            let common_target =
                                candidate.edge.target == source_candidate.edge.target;

                            let tracking_forward = common_source && common_target;

                            let source_percentage = source_candidate.percentage(ctx.map)?;
                            let target_percentage = candidate.percentage(ctx.map)?;

                            return if tracking_forward && source_percentage <= target_percentage {
                                // We are moving forward, it is simply the distance between the nodes
                                Some(Reachable::new(*source, *target, vec![]).distance_only())
                            } else {
                                // We are going "backwards", behaviour becomes dependent on
                                // the directionality of the edge. However, to return across the
                                // node is an independent transition, and is not covered.
                                break 'stmt;
                            };
                        }
                    }

                    // Generate the path to this target using the predicate map
                    let path_to_target = Self::path_builder(
                        &candidate.edge.source,
                        &source_candidate.edge.target,
                        &predicate_map,
                    )?;

                    let path = path_to_target
                        .windows(2)
                        .filter_map(|pair| {
                            if let [a, b] = pair {
                                return ctx.edge(a, b);
                            }

                            None
                        })
                        .collect::<Vec<_>>();

                    Some(Reachable::new(*source, *target, path))
                })
                .collect::<Vec<_>>()
        };

        Some(reachable)
    }
}

impl<N, E, M> Solver<E, M, N> for SelectiveForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M> + Send + Sync,
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
        let (start, end) = transition
            .candidates
            .attach_ends(&transition.layers)
            .map_err(MatchError::EndAttachFailure)?;

        debug!("Attached Ends");
        transition.candidates.weave(&transition.layers);
        debug!("Weaved all candidate layers.");

        info!("Solving: Start={start:?}. End={end:?}.");
        let context = transition.context(runtime);

        let num_layers = transition.layers.layers.len();

        // Pre-warm the predicate cache for every candidate's edge target across all
        // layers in parallel. Running one Dijkstra per unique road node per thread
        // eliminates the cold-miss latency that would otherwise serialize them in the
        // Viterbi pass. scc::HashMap handles concurrent inserts safely, so dedup is
        // unnecessary — duplicate queries on already-warm entries cost one O(1) lookup.
        transition.layers.layers.par_iter().for_each(|layer| {
            for &id in &layer.nodes {
                if let Some(c) = context.candidate(&id) {
                    self.predicate.query(&context, c.edge.target);
                }
            }
        });

        // dp[id] = (best_cumulative_cost, predecessor_id)
        let mut dp: FxHashMap<CandidateId, (u32, CandidateId)> = FxHashMap::default();
        let mut reachable_hash: FxHashMap<(usize, usize), Reachable<E>> = FxHashMap::default();

        // Initialise layer 0: cost = emission only, predecessor = start.
        if let Some(first_layer) = transition.layers.layers.first() {
            for &id in &first_layer.nodes {
                let emission = context.candidate(&id).map_or(u32::MAX, |c| c.emission);
                dp.insert(id, (emission, start));
            }
        }

        // Beam width: how many sources to expand per layer transition.
        // Top-BEAM_WIDTH candidates by cumulative cost are expanded; the rest are pruned.
        const BEAM_WIDTH: usize = 2;

        let layers = &transition.layers.layers;
        let heuristics = transition.heuristics;

        for layer_idx in 0..num_layers.saturating_sub(1) {
            let mut sources: Vec<(CandidateId, u32)> = layers[layer_idx]
                .nodes
                .iter()
                .filter_map(|&id| {
                    let &(cost, _) = dp.get(&id)?;
                    (cost < u32::MAX).then_some((id, cost))
                })
                .collect();
            sources.sort_unstable_by_key(|&(_, cost)| cost);
            sources.truncate(BEAM_WIDTH);

            let next_nodes = &layers[layer_idx + 1].nodes;

            for (source_id, source_cost) in sources {
                let reachable = self
                    .reachable(&context, &source_id, next_nodes)
                    .unwrap_or_default();

                for mut r in reachable {
                    let path_vec = r.path_nodes().collect_vec();

                    let Some(src) = context.candidate(&r.source) else {
                        continue;
                    };
                    let Some(tgt) = context.candidate(&r.target) else {
                        continue;
                    };

                    let optimal_path = Trip::new_with_map_and_offsets(
                        context.map,
                        &path_vec,
                        src.position,
                        tgt.position,
                    );

                    let (Some(sl), Some(tl)) = (
                        layers.get(src.location.layer_id),
                        layers.get(tgt.location.layer_id),
                    ) else {
                        continue;
                    };
                    let layer_width = Haversine.distance(sl.origin, tl.origin);

                    let transition_cost = heuristics.transition(TransitionContext {
                        map_path: &path_vec,
                        requested_resolution_method: r.resolution_method,
                        source_candidate: &r.source,
                        target_candidate: &r.target,
                        routing_context: &context,
                        layer_width,
                        optimal_path,
                    });

                    let new_cost = source_cost
                        .saturating_add(tgt.emission)
                        .saturating_add(transition_cost);

                    #[cfg(debug_assertions)]
                    {
                        r.cost = tgt.emission.saturating_add(transition_cost);
                    }

                    let entry = dp.entry(r.target).or_insert((u32::MAX, source_id));
                    if new_cost < entry.0 {
                        *entry = (new_cost, source_id);
                    }

                    reachable_hash.insert(r.hash(), r);
                }
            }
        }

        // Find the lowest-cost candidate in the final layer.
        let (best_final, best_cost) = layers
            .last()
            .and_then(|layer| {
                layer
                    .nodes
                    .iter()
                    .filter_map(|&id| dp.get(&id).map(|&(cost, _)| (id, cost)))
                    .filter(|&(_, cost)| cost < u32::MAX)
                    .min_by_key(|&(_, cost)| cost)
            })
            .ok_or(MatchError::CollapseFailure(CollapseError::NoPathFound))?;

        // Traceback: build [start, cand_0, ..., cand_{N-1}, best_final, end].
        let mut path = vec![best_final];
        let mut cur = best_final;
        loop {
            let &(_, pred) = dp
                .get(&cur)
                .ok_or(MatchError::CollapseFailure(CollapseError::NoPathFound))?;
            path.push(pred);
            if pred == start {
                break;
            }
            cur = pred;
        }
        path.reverse();
        path.push(end);

        info!("Total cost of solve: {best_cost}");

        let reached = path
            .windows(2)
            .filter_map(|nodes| {
                if let [a, b] = nodes {
                    reachable_hash.get(&(a.index(), b.index())).cloned()
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        // Update candidate graph with calculated weights (debug only).
        #[cfg(debug_assertions)]
        for (&(a_idx, b_idx), &Reachable { cost, .. }) in reachable_hash.iter() {
            let a = CandidateId::new(a_idx);
            let b = CandidateId::new(b_idx);

            if let Some(edge_idx) = transition.candidates.graph.find_edge(a, b) {
                if let Some(edge) = transition.candidates.graph.edge_weight_mut(edge_idx) {
                    edge.weight = cost;
                }
            }
        }

        Ok(CollapsedPath::new(
            best_cost,
            reached,
            path,
            transition.candidates,
            #[cfg(debug_assertions)]
            reachable_hash.values().cloned().collect(),
        ))
    }
}
