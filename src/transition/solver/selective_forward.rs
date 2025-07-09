use crate::transition::*;
use routers_codec::{Entry, Metadata};

use log::{debug, info};

use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::sync::{Arc, Mutex};

use geo::{Distance, Haversine};
use itertools::Itertools;
use measure_time::debug_time;
use pathfinding::num_traits::Zero;

/// A Upper-Bounded Dijkstra (UBD) algorithm.
///
/// TODO: Docs
pub struct SelectiveForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    // Internally holds a successors cache
    predicate: Arc<Mutex<PredicateCache<E, M>>>,
    reachable_hash: RefCell<FxHashMap<(usize, usize), Reachable<E>>>,
}

impl<E, M> Default for SelectiveForwardSolver<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn default() -> Self {
        Self {
            predicate: Arc::new(Mutex::new(PredicateCache::default())),
            reachable_hash: RefCell::new(FxHashMap::default()),
        }
    }
}

impl<E, M> SelectiveForwardSolver<E, M>
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
        (start, end): (CandidateId, CandidateId),
        source: &CandidateId,
    ) -> Vec<(CandidateId, CandidateEdge)>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M> + Send + Sync,
        'b: 'a,
    {
        let successors = transition.candidates.next_layer(source);

        // #[cold]
        if *source == start {
            // No cost to reach a first node.
            return successors
                .into_iter()
                .map(|candidate| (candidate, CandidateEdge::zero()))
                .collect::<Vec<_>>();
        }

        // Fast-track to the finish line
        if successors.contains(&end) {
            debug!("End-Successors: {successors:?}");
            return vec![(end, CandidateEdge::zero())];
        }

        let reachable = self
            .reachable(context, source, successors.as_slice())
            .unwrap_or_default();

        // Note: `reachable` ~= free, `reach` ~= 0.1ms (some overhead- how?)
        {
            debug_time!("Format Reachable Elements");
            let mut hash = self.reachable_hash.borrow_mut();

            reachable
                .into_iter()
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

                    hash.insert(reachable.hash(), reachable.clone());
                    Some((reachable.target, CandidateEdge::new(cost)))
                })
                .collect::<Vec<_>>()
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
        ctx: &'a RoutingContext<'a, E, M>,
        source: &CandidateId,
        targets: &'a [CandidateId],
    ) -> Option<Vec<Reachable<E>>> {
        let source_candidate = ctx.candidate(source)?;

        // Upper-Bounded reachable map containing a Child:Parent relation
        // Note: Parent is OsmEntryId::NULL, which will not be within the map,
        //       indicating the root element.
        let predicate_map = {
            debug_time!("query predicate for {source:?}");

            self.predicate
                .lock()
                .ok()?
                .query(ctx, source_candidate.edge.target)
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

impl<E, M> Solver<E, M> for SelectiveForwardSolver<E, M>
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
        debug!("Attached Ends");
        transition.candidates.weave(&transition.layers);
        debug!("Weaved all candidate layers.");

        info!("Solving. ");
        let _context = transition.context(runtime);

        // Note: For every candidate, generate their reachable elements, then run the solver overtop.
        //       This means we can do it in parallel, which is more efficient - however will have to
        //       compute for *every* candidate, not just the likely ones, which will lead to poor
        //       scalability for really long-routes.
        //
        //       This behaviour can be implemented using the `AllForwardSolver` going forward.

        // call collapse
        unimplemented!();
    }
}
