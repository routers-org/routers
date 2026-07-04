//! Eager forward solver backed by [`routers_trellis`].
//!
//! Materialises every layer-to-layer transition into a dense trellis matrix, then
//! solves the minimum-cost path with a Viterbi DP (`routers_trellis::ViterbiSolver`)
//! instead of an `astar` over a candidate graph.
//!
//! This is the "eager" driver of `SPEC.md` §3b/§4a. It is additive: the candidate
//! store (`Candidates`) is reused as-is, and the layer node vectors double as the
//! `(LayerId, NodeId) -> CandidateId` coordinate table (`Layer.nodes`).
//!
//! ## Cost model (SPEC §O3)
//! Trellis cost lives only on edges and layer 0 seeds at 0, so emission is folded
//! into the boundary cells:
//! ```text
//! cell[k][from][to] = transition(from->to) + emission(to)
//!                   + if k == 0 { emission(from) } else { 0 }
//! ```
//! counting every layer's emission exactly once. Cells are clamped to `MAX_WEIGHT`
//! (SPEC §R1) since trellis rejects larger weights where the old graph saturated.

use std::{marker::PhantomData, sync::Arc};

use crate::{
    CollapseError, CollapsedPath, Costing, MatchError, PredicateCache, Reachable, Solver,
    Transition, TransitionContext,
    candidate::CandidateId,
    costing::{EmissionStrategy, TransitionStrategy},
};

use itertools::Itertools;
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{LayerId, MAX_WEIGHT, NO_EDGE, Solve, Trellis, ViterbiSolver};
use rustc_hash::FxHashMap;

use super::expansion::reachable_between;

/// Eager, dense, Viterbi-backed forward solver. See module docs.
pub struct TrellisForwardSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    predicate: Arc<PredicateCache<E, M, N>>,
    _phantom: PhantomData<N>,
}

impl<E, M, N> Default for TrellisForwardSolver<E, M, N>
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

impl<E, M, N> TrellisForwardSolver<E, M, N>
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
}

impl<E, M, N> Solver<E, M, N> for TrellisForwardSolver<E, M, N>
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
        let context = transition.context(runtime);
        let layers = &transition.layers.layers;
        let n = layers.len();

        // Widths come straight from the per-layer candidate counts.
        let widths: Vec<u32> = layers.iter().map(|l| l.nodes.len() as u32).collect();
        let mut trellis = Trellis::new(widths)
            .map_err(|_| MatchError::CollapseFailure(CollapseError::NoPathFound))?;

        // Per-edge side-data, keyed by the global CandidateId pair (SPEC §D2/§5).
        let mut side: FxHashMap<(CandidateId, CandidateId), Reachable<E>> = FxHashMap::default();

        for k in 0..n.saturating_sub(1) {
            let cur = &layers[k].nodes;
            let nxt = &layers[k + 1].nodes;
            let nw = nxt.len();
            let mut rows = vec![NO_EDGE; cur.len() * nw];

            for (i, src) in cur.iter().enumerate() {
                // First-layer emission has nowhere else to live (SPEC §O3).
                let src_emission = if k == 0 {
                    context.candidate(src).map(|c| c.emission).unwrap_or(0)
                } else {
                    0
                };

                for (j, tgt) in nxt.iter().enumerate() {
                    let Some(reachable) = reachable_between(&context, &self.predicate, src, tgt)
                    else {
                        continue; // unreachable -> leave NO_EDGE
                    };
                    let Some(target) = context.candidate(tgt) else {
                        continue;
                    };

                    let path_vec = reachable.path_nodes().collect_vec();
                    let Some(tctx) =
                        TransitionContext::new(&context, reachable.candidates(), &path_vec)
                    else {
                        continue;
                    };
                    let tctx = tctx.with_resolution_method(reachable.resolution_method);
                    let transition_cost = transition.heuristics.transition(tctx);

                    let cost = target
                        .emission
                        .saturating_add(transition_cost)
                        .saturating_add(src_emission)
                        .min(MAX_WEIGHT); // SPEC §R1: restore saturating semantics at the fill boundary

                    rows[i * nw + j] = cost;
                    side.insert((*src, *tgt), reachable);
                }
            }

            trellis
                .fill_transition(LayerId(k as u32), &rows)
                .map_err(|_| MatchError::CollapseFailure(CollapseError::NoPathFound))?;
        }

        // `context` is not used past the fill loop, so NLL ends its borrow of
        // `transition` here — leaving `transition.candidates` free to move below.
        let path = ViterbiSolver::new()
            .solve(&trellis)
            .map_err(|_| MatchError::CollapseFailure(CollapseError::NoPathFound))?;

        if !path.reachable {
            return Err(MatchError::CollapseFailure(CollapseError::NoPathFound));
        }

        // Map layer-local NodeIds back to CandidateIds via the coord table.
        let route: Vec<CandidateId> = path
            .nodes
            .iter()
            .enumerate()
            .filter_map(|(k, nid)| layers.get(k)?.nodes.get(nid.0 as usize).copied())
            .collect();

        // Exactly one Reachable per real hop; no virtual source/sink to exclude
        // (trellis has none). SPEC §R3.
        let reached: Vec<Reachable<E>> = route
            .windows(2)
            .filter_map(|w| match w {
                [a, b] => side.get(&(*a, *b)).cloned(),
                _ => None,
            })
            .collect();

        #[cfg(debug_assertions)]
        let considered: Vec<Reachable<E>> = side.values().cloned().collect();

        Ok(CollapsedPath::new(
            path.cost,
            reached,
            route,
            transition.candidates,
            #[cfg(debug_assertions)]
            considered,
        ))
    }
}
