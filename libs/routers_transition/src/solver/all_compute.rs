//! All-compute forward solver — weighs *every* reachable transition.
//!
//! The exhaustive end of the compute axis: [`select`](Solver::select) returns the
//! whole next layer, so every boundary is filled densely before the trellis graph
//! solve. Exact, and the fastest choice for a from-scratch match. See
//! [`SelectiveSolver`](crate::SelectiveSolver) for the pruned counterpart.

use std::{marker::PhantomData, sync::Arc};

use crate::{Candidate, CandidateId, PredicateCache, RoutingContext, Solver};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::NodeId;

/// Weighs every reachable transition. Inherits the full [`Solver`] pipeline.
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
    fn cache(&self) -> &PredicateCache<E, M, N> {
        &self.predicate
    }

    fn select(
        &self,
        _ctx: &RoutingContext<E, M, N>,
        _source: &Candidate<E>,
        to_layer: &[CandidateId],
    ) -> Vec<NodeId> {
        (0..to_layer.len() as u32).map(NodeId).collect()
    }
}
