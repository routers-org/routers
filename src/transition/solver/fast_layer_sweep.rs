use crate::transition::*;

use log::info;

use std::sync::{Arc, Mutex};

use geo::{Distance, Haversine};
use itertools::Itertools;
use pathfinding::prelude::*;
use routers_codec::{Entry, Metadata};

use rayon::iter::ParallelIterator;
use rayon::prelude::ParallelSlice;

/// A Upper-Bounded Dijkstra (UBD) algorithm.
///
/// TODO: Docs
pub struct FastLayerSweepSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    // Internally holds a successors cache
    predicate: Arc<Mutex<PredicateCache<E, M>>>,
    reachable_hash: scc::HashMap<(usize, usize), Reachable<E>>,
}

impl<E, M> Default for FastLayerSweepSolver<E, M>
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

impl<E, M> FastLayerSweepSolver<E, M>
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

    fn optimality(&self, a: &Layer, b: &Layer) -> Vec<Reachable<E>> {
        // Find K optimal paths between layer A and layer B.
        // => Create a virtual node that represents the entry and terminus of A and B.

        vec![]
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
        info!("Solving. ");
        let context = transition.context(runtime);

        // For every layer pair in the transition set, iterate and find optimal paths between.
        let all_reachable = transition
            .layers
            .par_windows(2)
            .flat_map(|entries| {
                if let [a, b] = entries {
                    return self.optimality(a, b);
                }

                vec![]
            })
            .filter_map(|reachable| {
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
                    routing_context: &context,

                    layer_width,
                    optimal_path,
                });

                Some((reachable, CandidateEdge::new(transition_cost)))
            })
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
