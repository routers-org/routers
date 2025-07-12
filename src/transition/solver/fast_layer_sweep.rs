use crate::transition::*;

use log::{debug, info};

use std::sync::{Arc, Mutex, RwLock};

use geo::{Distance, Haversine};
use itertools::Itertools;
use measure_time::{debug_time, info_time};
use pathfinding::num_traits::Zero;
use routers_codec::{Entry, Metadata};

use rayon::iter::ParallelIterator;
use rayon::prelude::ParallelSlice;
use routers_codec::osm::primitives::TransportMode::Canoe;
use scc::HashMap;

/// A Upper-Bounded Dijkstra (UBD) algorithm.
///
/// TODO: Docs
pub struct FastLayerSweepSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    // Internally holds a successors cache
    successors: Arc<Mutex<SuccessorsCache<E, M>>>,
    reachable_hash: scc::HashMap<(usize, usize), Reachable<E>>,
}

impl<E, M> Default for FastLayerSweepSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn default() -> Self {
        Self {
            successors: Arc::new(Mutex::new(SuccessorsCache::default())),
            reachable_hash: scc::HashMap::new(),
        }
    }
}

impl<E, M> FastLayerSweepSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn use_cache(self, cache: Arc<Mutex<SuccessorsCache<E, M>>>) -> Self {
        Self {
            successors: cache,
            ..self
        }
    }

    fn optimality<'a>(
        &self,
        ctx: &RoutingContext<E, M>,
        a: &'a Layer,
        b: &'a Layer,
    ) -> Vec<Reachable<E>>
    where
        E: 'a,
    {
        debug_time!("optimality");

        const NUM_SHORTEST_PATHS: usize = 1;

        let mut successors = |&node: &E| {
            // let mut cache = self.successors.lock().unwrap();

            ArcIter::new(SuccessorsCache::default().query(ctx, node))
                .filter(|(_, edge, _)| {
                    // Only traverse paths which can be accessed by
                    // the specific runtime routing conditions available
                    let meta = ctx.map.meta(edge);
                    let direction = edge.direction();

                    meta.accessible(ctx.runtime, direction)
                })
                .map(|(a, _, b)| (a, b))
                .filter(|(_, weight)| weight.1 < 20_000)
        };

        let bridge = Bridge::new(E::start_id(), E::end_id()).layered(a, b);

        let start_candidates = a
            .nodes
            .iter()
            .filter_map(|candidate| Some((ctx.candidate(candidate)?.edge.source, *candidate)))
            .collect::<HashMap<_, _>>();

        let end_candidates = b
            .nodes
            .iter()
            .filter_map(|candidate| Some((ctx.candidate(candidate)?.edge.source, *candidate)))
            .collect::<HashMap<_, _>>();

        let shortest = pathfinding::directed::yen::yen(
            &E::start_id(),
            |node| {
                // If the node is the same as the bridge's start, it's free to go to the entering layer.
                if *node == E::start_id() {
                    return a
                        .nodes
                        .iter()
                        .filter_map(|node| {
                            let candidate = ctx.candidate(node)?;
                            Some((candidate.edge.source, WeightAndDistance::zero()))
                        })
                        .collect();
                }

                // If the node is within the departing layer, it's free to leave (go to the terminus).
                if end_candidates.contains(node) {
                    return vec![(E::end_id(), WeightAndDistance::zero())];
                }

                return successors(node).collect_vec();
            },
            |&maybe_end| maybe_end == E::end_id(),
            NUM_SHORTEST_PATHS,
        );

        shortest
            .into_iter()
            .filter_map(|(nodes, weight)| {
                debug_assert_eq!(*nodes.first().unwrap(), E::start_id());
                debug_assert_eq!(*nodes.last().unwrap(), E::end_id());

                let entering_layer = nodes.get(1)?;
                let departing_layer = nodes.get(nodes.len() - 2)?;

                let start_candidate = start_candidates.get(entering_layer)?;
                let end_candidate = end_candidates.get(departing_layer)?;

                let path = nodes
                    .windows(2)
                    .filter_map(|pair| {
                        if let [a, b] = pair {
                            return ctx.edge(a, b);
                        }

                        None
                    })
                    .collect::<Vec<_>>();

                Some(Reachable::new(*start_candidate, *end_candidate, path))
            })
            .collect::<Vec<_>>()
    }
}

impl<E, M> Solver<E, M> for FastLayerSweepSolver<E, M>
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
        info_time!("FastLayerSweep solve");
        let context = transition.context(runtime);

        // For every layer pair in the transition set, iterate and find optimal paths between.
        let all_reachable = transition
            .layers
            .par_windows(2)
            .flat_map(|entries| {
                if let [a, b] = entries {
                    return self.optimality(&context, a, b);
                }

                vec![]
            })
            .filter_map(|reachable| transition.resolve(&context, reachable))
            .collect::<Vec<_>>();

        // Add all the costs into the graph, adding appropriate
        // connecting edges and respective weights.
        {
            // Inner-scope for drop-protection
            let mut writable_graph = transition.candidates.graph.write().unwrap();

            all_reachable.into_iter().for_each(|(reachable, cost)| {
                writable_graph.add_edge(reachable.source, reachable.target, cost);
            });
        }

        transition.collapse(&self.reachable_hash)
    }
}
