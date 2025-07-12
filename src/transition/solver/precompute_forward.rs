use crate::transition::*;

use log::info;

use rustc_hash::FxHashMap;
use std::sync::{Arc, Mutex};

use measure_time::debug_time;
use routers_codec::{Entry, Metadata};

use rayon::iter::ParallelIterator;
use rayon::prelude::IntoParallelRefIterator;

/// A Upper-Bounded Dijkstra (UBD) algorithm.
///
/// TODO: Docs
pub struct PrecomputeForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    // Internally holds a successors cache
    predicate: Arc<Mutex<PredicateCache<E, M>>>,
    reachable_hash: scc::HashMap<(usize, usize), Reachable<E>>,
}

impl<E, M> Default for PrecomputeForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn default() -> Self {
        Self {
            predicate: Arc::new(Mutex::new(PredicateCache::default())),
            reachable_hash: scc::HashMap::new(),
        }
    }
}

impl<E, M> PrecomputeForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn use_cache(self, cache: Arc<Mutex<PredicateCache<E, M>>>) -> Self {
        Self {
            predicate: cache,
            ..self
        }
    }

    fn reach<'a, 'b, Emmis, Trans>(
        &'b self,
        transition: &'b Transition<'b, Emmis, Trans, E, M>,
        context: &'b RoutingContext<'b, E, M>,
        source: &CandidateId,
    ) -> Vec<(Reachable<E>, CandidateEdge)>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M> + Send + Sync,
        'b: 'a,
    {
        use rayon::prelude::{IntoParallelIterator, ParallelIterator};

        let layer = transition.candidates.next_layer(source);

        layer
            .into_par_iter()
            .filter_map(|target| self.get_reachable(context, source, &target))
            .filter_map(move |reachable| transition.resolve(context, reachable))
            .collect::<Vec<_>>()
    }

    fn get_reachable<'a>(
        &self,
        ctx: &'a RoutingContext<'a, E, M>,
        source_id: &CandidateId,
        target_id: &CandidateId,
    ) -> Option<Reachable<E>> {
        let source = ctx.candidate(source_id)?;

        // Upper-Bounded reachable map containing a Child:Parent relation
        // Note: Parent is OsmEntryId::NULL, which will not be within the map,
        //       indicating the root element.
        let predicate_map = { self.predicate.lock().ok()?.query(ctx, source.edge.target) };

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

impl<E, M> Solver<E, M> for PrecomputeForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M> + Send + Sync,
    {
        info!("Solving. ");
        let context = transition.context(runtime);

        // Pre-generate KV pair
        let _pair = {
            debug_time!("generate transition graph");

            transition
                .layers
                .layers
                .par_iter()
                .flat_map(|layer| {
                    // objectively O(n^2) / O(n) isn't going to scale; we need something more efficient...
                    // we need a way to do a multicast N:N from all in layer N to all in layer N+1
                    layer.nodes.par_iter().map(|source| {
                        let found = self.reach(&transition, &context, source);

                        let some = found
                            .into_iter()
                            .map(|(reachable, edge)| {
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

        // Note: For every candidate, generate their reachable elements, then run the solver overtop.
        //       This means we can do it in parallel, which is more efficient - however will have to
        //       compute for *every* candidate, not just the likely ones, which will lead to poor
        //       scalability for really long-routes.
        //
        //       This behaviour can be implemented using the `AllForwardSolver` going forward.

        // call collapse...
        unimplemented!();

        // We should generate the layers first. Over which we
        // should paralellise a .window function which is tasked
        // with creating the K shortest paths between two consecutive
        // candidates...
        //
        // With this, we can perform a bidirectional modified reach
        // dijkstra algorithm to perform the search to reduce the
        // search space consumed, prevent traversing largely unplausable
        // candidates, etc.
        //
        // This solver (in its current form) should be preserved
        // and rebranded as a slower but methodical solver which checks
        // every possible pathing sequence as it precomputes all paths.
        //
        // Therefore, we will ideally search far less space, and in the final
        // dijkstra path through the trajectory graph we can reveal the optimal
        // matchings of the trajectory.
        //
        // If this is true, we can hot-cache (bin heap) the candidates and paths between
        // such that if the service is hit with a consecutive request with similar
        // starting parameters it only has to compute the final layer's candidates
        // and the final window entry, all others would have a cache hit!
    }
}
