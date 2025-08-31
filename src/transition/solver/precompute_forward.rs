use crate::transition::*;

use log::{debug, info};

use rustc_hash::FxHashMap;

use geo::{Distance, Haversine};
use itertools::Itertools;
use measure_time::debug_time;
use pathfinding::num_traits::Zero;
use pathfinding::prelude::*;
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
    predicate: PredicateCache<E, M>,
    reachable_hash: scc::HashMap<(usize, usize), Reachable<E>>,
}

impl<E, M> Default for PrecomputeForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn default() -> Self {
        Self {
            predicate: PredicateCache::default(),
            reachable_hash: scc::HashMap::new(),
        }
    }
}

impl<E, M> PrecomputeForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn use_cache(self, cache: PredicateCache<E, M>) -> Self {
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

                    layer_width,
                    optimal_path,
                });

                let transition = (transition_cost as f64 * 0.6) as u32;
                let emission = (target.emission as f64 * 0.4) as u32;
                let cost = emission.saturating_add(transition);

                Some((reachable, CandidateEdge::new(cost)))
            })
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

impl<E, M> Solver<E, M> for PrecomputeForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn solve<Emmis, Trans>(
        &self,
        mut transition: Transition<Emmis, Trans, E, M>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M> + Send + Sync,
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

        // Pre-generate KV pair
        let mut pair = {
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

        pair.insert(
            start,
            transition.layers.layers[0]
                .nodes
                .iter()
                .map(|source| (*source, CandidateEdge::zero()))
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

        Ok(CollapsedPath::new(
            cost.weight,
            reached,
            path,
            transition.candidates,
        ))
    }
}
