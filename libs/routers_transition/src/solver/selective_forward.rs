use crate::{
    CollapseError, CollapsedPath, Costing, MatchError, PredicateCache, Reachable, Solver,
    TransitionContext, candidate::CandidateId, costing::{EmissionStrategy, TransitionStrategy},
    entity::Transition, primitives::RoutingContext,
};
use routers_network::{Entry, Metadata, Network};

use log::info;

use core::cell::RefCell;
use rustc_hash::FxHashMap;
use std::{marker::PhantomData, sync::Arc};

use itertools::Itertools;
use measure_time::debug_time;
use pathfinding::prelude::*;

/// Synthetic source node: connects (at emission cost) to every first-layer
/// candidate. Not a real candidate, so it is absent from the lookup and is
/// filtered out of the reconstructed route.
const START: CandidateId = CandidateId(u32::MAX);
/// Synthetic sink node: every last-layer candidate connects to it at zero cost.
const END: CandidateId = CandidateId(u32::MAX - 1);

/// A lazy, Upper-Bounded-Dijkstra (UBD) forward solver.
///
/// Explores the layered candidate structure on demand with `pathfinding::astar`,
/// only expanding (`reach`ing) candidates the frontier actually visits — so it
/// avoids materialising the full N:N transition set on long routes. Contrast with
/// the eager [`TrellisForwardSolver`](crate::TrellisForwardSolver).
pub struct SelectiveForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    // Internally holds a successors cache
    predicate: Arc<PredicateCache<E, M, N>>,
    reachable_hash: RefCell<FxHashMap<(usize, usize), Reachable<E>>>,

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
            reachable_hash: RefCell::new(FxHashMap::default()),
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

    /// Successors of `source` and their edge costs, for the astar frontier.
    ///
    /// Handles the synthetic [`START`]/[`END`] endpoints implicitly (there is no
    /// explicit graph): `START` fans out to the first layer at emission cost, the
    /// last layer connects to `END` at zero cost, and interior candidates expand to
    /// the next layer with `emission(target) + transition_cost` — stashing each
    /// [`Reachable`] for later reconstruction.
    fn reach<Emmis, Trans>(
        &self,
        transition: &Transition<'_, Emmis, Trans, E, M, N>,
        context: &RoutingContext<'_, E, M, N>,
        source: &CandidateId,
    ) -> Vec<(CandidateId, u32)>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        // Virtual source → every first-layer candidate, carrying its emission cost.
        if *source == START {
            return transition
                .candidates
                .coords
                .first()
                .map(|layer0| {
                    layer0
                        .iter()
                        .filter_map(|c| context.candidate(c).map(|cand| (*c, cand.emission)))
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
        }

        let Some(src_cand) = context.candidate(source) else {
            return Vec::new();
        };

        // Last real layer → virtual sink at zero cost.
        let last_layer = transition.layers.layers.len().saturating_sub(1);
        if src_cand.location.layer_id >= last_layer {
            return vec![(END, 0)];
        }

        let successors = transition.candidates.next_layer(source);
        let reachable = self
            .reachable(context, source, successors.as_slice())
            .unwrap_or_default();

        debug_time!("Format Reachable Elements");
        let mut hash = self.reachable_hash.borrow_mut();

        reachable
            .into_iter()
            .filter_map(move |mut reachable| {
                let path_vec = reachable.path_nodes().collect_vec();
                let target = context.candidate(&reachable.target)?;

                let transition_ctx =
                    TransitionContext::new(context, reachable.candidates(), &path_vec)?
                        .with_resolution_method(reachable.resolution_method);

                let transition_cost = transition.heuristics.transition(transition_ctx);
                let cost = target.emission.saturating_add(transition_cost);
                #[cfg(debug_assertions)]
                {
                    reachable.cost = cost;
                }

                hash.insert(reachable.hash(), reachable.clone());
                Some((reachable.target, cost))
            })
            .collect::<Vec<_>>()
    }

    /// Derives which candidates are reachable from `source`, and by which routed
    /// path, delegating to the shared expansion core (SPEC §O1).
    fn reachable<'a>(
        &self,
        ctx: &'a RoutingContext<'a, E, M, N>,
        source: &CandidateId,
        targets: &'a [CandidateId],
    ) -> Option<Vec<Reachable<E>>> {
        // Fail fast if the source candidate is unknown (preserves prior behaviour).
        ctx.candidate(source)?;

        let reachable = {
            debug_time!("predicates {source:?} -> reachable");

            // The predicate cache is read-through and keyed by the source edge, so
            // the per-target queries collapse to one computation + cache hits.
            targets
                .iter()
                .filter_map(|target| {
                    super::expansion::reachable_between(ctx, &self.predicate, source, target)
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
    N: Network<E, M>,
{
    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        info!("Solving (selective): start={START:?} end={END:?}");
        let context = transition.context(runtime);

        let Some((path, cost)) = ({
            debug_time!("Solved transition graph");

            astar(
                &START,
                |source| self.reach(&transition, &context, source),
                |_| 0u32,
                |node| *node == END,
            )
        }) else {
            return Err(MatchError::CollapseFailure(CollapseError::NoPathFound));
        };

        info!("Total cost of solve: {cost}");
        let reached = path
            .windows(2)
            .filter_map(|nodes| match nodes {
                [a, b] => self
                    .reachable_hash
                    .borrow()
                    .get(&(a.index(), b.index()))
                    .cloned(),
                _ => None,
            })
            .collect::<Vec<_>>();

        // `context` is unused past this point, so NLL ends its borrow of
        // `transition`, freeing `transition.candidates` to move below.
        Ok(CollapsedPath::new(
            cost,
            reached,
            path,
            transition.candidates,
            #[cfg(debug_assertions)]
            self.reachable_hash.borrow().values().cloned().collect(),
        ))
    }
}
