//! Selective forward solver — weighs only a pruned subset of transitions.
//!
//! [`select`](Solver::select) keeps, per source, the `fanout` next-layer
//! candidates nearest by straight-line distance, cutting the O(N²) reachability
//! computation per boundary to O(N·fanout). Everything else is inherited from the
//! [`Solver`] pipeline.
//!
//! Because it never re-weighs resolved boundaries, it is the natural choice for
//! extending a partially-solved trellis (e.g. streaming a growing trip) rather
//! than a from-scratch match.
//!
//! # Exactness
//! Proximity pruning is a heuristic: a far-but-optimal target can be missed. For
//! guaranteed-exact matching use [`AllComputeSolver`](crate::AllComputeSolver).

use alloc::sync::Arc;
use core::marker::PhantomData;

use crate::{Candidate, CandidateId, PredicateCache, RoutingContext, Solver};
use geo::{Distance, Haversine};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::NodeId;

/// Default per-source fan-out: how many nearest next-layer candidates to weigh.
pub const DEFAULT_FANOUT: usize = 16;

/// Weighs the `fanout` nearest next-layer candidates per source. Inherits the
/// full [`Solver`] pipeline.
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
    fn cache(&self) -> &PredicateCache<E, M, N> {
        &self.predicate
    }

    fn select(
        &self,
        ctx: &RoutingContext<E, M, N>,
        source: &Candidate<E>,
        to_layer: &[CandidateId],
    ) -> Vec<NodeId> {
        let distance_to = |target: &CandidateId| {
            ctx.candidate(target)
                .map(|c| Haversine.distance(source.position, c.position))
                .unwrap_or(f64::INFINITY)
        };

        let mut nearest = (0..to_layer.len()).collect::<Vec<_>>();
        if nearest.len() > self.fanout {
            nearest
                .sort_by(|&a, &b| distance_to(&to_layer[a]).total_cmp(&distance_to(&to_layer[b])));
            nearest.truncate(self.fanout);
        }

        nearest.into_iter().map(|i| NodeId(i as u32)).collect()
    }
}
