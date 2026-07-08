//! Selective forward solver.
//!
//! Same trellis-backed pipeline as [`AllComputeSolver`](crate::AllComputeSolver),
//! but *selective* about which transitions it computes: for each source candidate
//! it only weighs the `fanout` geometrically-nearest candidates in the next layer,
//! leaving the rest `NO_EDGE`. This turns the O(N²) reachability computation per
//! boundary into O(N·fanout) — the win the old upper-bounded-Dijkstra solver
//! sought, expressed as a sparse trellis fill.
//!
//! It respects trellis semantics (only fills pending boundaries) and uses the
//! default [`Solver::solve`] (Viterbi + reconstruct), so it composes with a
//! partially-solved trellis exactly like the all-compute solver.
//!
//! # Exactness
//! Pruning by geometric proximity is a heuristic: on pathological layers the true
//! optimum could involve a far target and be missed. `fanout` trades exactness for
//! speed; the default is generous. For guaranteed-exact matching use
//! [`AllComputeSolver`](crate::AllComputeSolver).

use std::{marker::PhantomData, sync::Arc};

use crate::{
    CollapseError, MatchError, PredicateCache, Reachable, SideTable, Solver, Transition,
    candidate::CandidateId,
    costing::{EmissionStrategy, TransitionStrategy},
};

use geo::{Distance, Haversine};
use rayon::prelude::*;
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{LayerId, NO_EDGE, Trellis};

use super::all_compute::weigh_edge;
use super::expansion::reachable_between;

/// Default per-source fan-out: how many nearest next-layer candidates to weigh.
pub const DEFAULT_FANOUT: usize = 16;

/// Selective (pruned) parallel weigher. Uses the default trellis graph solve.
pub struct SelectiveSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    predicate: Arc<PredicateCache<E, M, N>>,
    fanout: usize,
    _phantom: PhantomData<N>,
}

impl<E, M, N> Default for SelectiveSolver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    fn default() -> Self {
        Self {
            predicate: Arc::new(PredicateCache::default()),
            fanout: DEFAULT_FANOUT,
            _phantom: PhantomData,
        }
    }
}

impl<E, M, N> SelectiveSolver<E, M, N>
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

    /// Override how many nearest next-layer candidates are weighed per source.
    pub fn with_fanout(self, fanout: usize) -> Self {
        Self { fanout, ..self }
    }
}

impl<E, M, N> Solver<E, M, N> for SelectiveSolver<E, M, N>
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
            if trellis.is_resolved(LayerId(k as u32)) {
                continue;
            }

            let cur = &layers[k].nodes;
            let nxt = &layers[k + 1].nodes;
            let nw = nxt.len();

            let per_source: Vec<Vec<(usize, u32, Reachable<E>)>> = cur
                .par_iter()
                .enumerate()
                .map(|(i, src)| {
                    let Some(src_cand) = context.candidate(src) else {
                        return Vec::new();
                    };
                    let src_emission = if k == 0 { src_cand.emission } else { 0 };

                    // Select the `fanout` nearest next-layer candidates (by
                    // straight-line distance) — the only ones we pay `reach` for.
                    let mut ranked: Vec<(usize, &CandidateId)> = nxt.iter().enumerate().collect();
                    if ranked.len() > self.fanout {
                        ranked.sort_by(|(_, a), (_, b)| {
                            let da = context
                                .candidate(a)
                                .map(|c| Haversine.distance(src_cand.position, c.position))
                                .unwrap_or(f64::INFINITY);
                            let db = context
                                .candidate(b)
                                .map(|c| Haversine.distance(src_cand.position, c.position))
                                .unwrap_or(f64::INFINITY);
                            da.total_cmp(&db)
                        });
                        ranked.truncate(self.fanout);
                    }

                    ranked
                        .into_iter()
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
