use crate::transition::*;

use itertools::Itertools;
use measure_time::{debug_time, info_time};
use pathfinding::num_traits::Zero;
use routers_codec::{Entry, Metadata};

use rayon::iter::ParallelIterator;
use rayon::prelude::ParallelSlice;
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
    successors: SuccessorsCache<E, M>,
    reachable_hash: scc::HashMap<(usize, usize), Reachable<E>>,
}

impl<E, M> Default for FastLayerSweepSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn default() -> Self {
        Self {
            successors: SuccessorsCache::default(),
            reachable_hash: scc::HashMap::new(),
        }
    }
}

impl<E, M> FastLayerSweepSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn use_cache(self, cache: SuccessorsCache<E, M>) -> Self {
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


        // Algo is running recursively, in resolving the shortest path (possibly cyclically induced)
        // which is therefore allocating infinitely and reaching a memory hit before a stack smash
        // since it appears to be TCOd and isnt allocating a significant amount more stack frame space.
        let successors = |&node: &E| {
            let suc: Vec<(E, WeightAndDistance)> = self.successors.query(ctx, node)
                .iter()
                .filter(|(_, edge, _)| {
                    // Only traverse paths which can be accessed by
                    // the specific runtime routing conditions available
                    let meta = ctx.map.meta(edge);
                    let direction = edge.direction();

                    meta.accessible(ctx.runtime, direction)
                })
                .map(|(a, _, b)| (*a, *b))
                .collect::<_>();

            eprintln!("suc.len() = {}", suc.len());
            return suc;
        };

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

                successors(node)
            },
            |&maybe_end| maybe_end == E::end_id(),
            NUM_SHORTEST_PATHS,
        );

        // SAFE.
        shortest
            .into_iter()
            .filter_map(|(nodes, _)| {
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
            .inspect(|reachable| {
                self.reachable_hash
                    .insert(reachable.hash(), reachable.clone())
                    .expect("hash collision, must insert correctly.");
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

        // TODO: panic!("solve midway");

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
