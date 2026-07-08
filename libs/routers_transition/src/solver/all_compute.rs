//! All-compute forward solver.
//!
//! Fills *every* pending transition of the caller-owned trellis (in parallel),
//! then relies on the trait's default [`Solver::solve`] to run the trellis graph
//! solve and reconstruct. This is the exhaustive end of the compute axis; see
//! [`SelectiveSolver`](crate::SelectiveSolver) for the pruned counterpart.
//!
//! ## Cost model (SPEC §O3)
//! Trellis cost lives only on edges and layer 0 seeds at 0, so emission is folded
//! into the boundary cells:
//! ```text
//! cell[k][from][to] = transition(from->to) + emission(to)
//!                   + if k == 0 { emission(from) } else { 0 }
//! ```
//! Cells are clamped to `MAX_WEIGHT` (SPEC §R1).

use std::{marker::PhantomData, sync::Arc};

use crate::{
    CollapseError, Costing, MatchError, PredicateCache, Reachable, SideTable, Solver, Transition,
    TransitionContext,
    costing::{EmissionStrategy, TransitionStrategy},
};

use itertools::Itertools;
use rayon::prelude::*;
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{LayerId, MAX_WEIGHT, NO_EDGE, Trellis};

use super::expansion::reachable_between;

/// Eager, dense, parallel weigher. Uses the default trellis graph solve.
pub struct AllComputeSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    predicate: Arc<PredicateCache<E, M, N>>,
    _phantom: PhantomData<N>,
}

impl<E, M, N> Default for AllComputeSolver<E, M, N>
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

impl<E, M, N> AllComputeSolver<E, M, N>
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

impl<E, M, N> Solver<E, M, N> for AllComputeSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    fn weigh<Emmis, Trans>(
        &self,
        transition: &Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
        trellis: &mut Trellis,
        side: &mut SideTable<E>,
    ) -> Result<(), MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        let context = transition.context(runtime);
        let layers = &transition.layers.layers;

        for k in 0..layers.len().saturating_sub(1) {
            // Trellis semantics: leave already-resolved boundaries untouched.
            if trellis.is_resolved(LayerId(k as u32)) {
                continue;
            }

            let cur = &layers[k].nodes;
            let nxt = &layers[k + 1].nodes;
            let nw = nxt.len();

            // Parallel over source candidates; each yields its filled cells.
            let per_source: Vec<Vec<(usize, u32, Reachable<E>)>> = cur
                .par_iter()
                .enumerate()
                .map(|(i, src)| {
                    // First-layer emission has nowhere else to live (SPEC §O3).
                    let src_emission = if k == 0 {
                        context.candidate(src).map(|c| c.emission).unwrap_or(0)
                    } else {
                        0
                    };

                    nxt.iter()
                        .enumerate()
                        .filter_map(|(j, tgt)| {
                            let reachable = reachable_between(&context, &self.predicate, src, tgt)?;
                            let cost = weigh_edge(&context, transition, &reachable, src_emission)?;
                            Some((i * nw + j, cost, reachable))
                        })
                        .collect::<Vec<_>>()
                })
                .collect();

            let mut rows = vec![NO_EDGE; cur.len() * nw];
            for (idx, cost, reachable) in per_source.into_iter().flatten() {
                rows[idx] = cost;
                side.insert((reachable.source, reachable.target), reachable);
            }

            trellis
                .fill_transition(LayerId(k as u32), &rows)
                .map_err(|_| MatchError::CollapseFailure(CollapseError::NoPathFound))?;
        }

        Ok(())
    }
}

/// Cost of one transition edge: `emission(to) + transition_cost + src_emission`,
/// clamped to `MAX_WEIGHT` (SPEC §O3/§R1). Shared by the compute strategies.
pub(super) fn weigh_edge<Emmis, Trans, E, M, N>(
    context: &crate::RoutingContext<'_, E, M, N>,
    transition: &Transition<'_, Emmis, Trans, E, M, N>,
    reachable: &Reachable<E>,
    src_emission: u32,
) -> Option<u32>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E> + Send + Sync,
{
    let target = context.candidate(&reachable.target)?;
    let path_vec = reachable.path_nodes().collect_vec();
    let tctx = TransitionContext::new(context, reachable.candidates(), &path_vec)?
        .with_resolution_method(reachable.resolution_method);
    let transition_cost = transition.heuristics.transition(tctx);

    Some(
        target
            .emission
            .saturating_add(transition_cost)
            .saturating_add(src_emission)
            .min(MAX_WEIGHT),
    )
}
