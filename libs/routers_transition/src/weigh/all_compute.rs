//! All-compute weigher weighs *every* reachable transition, hence the name.
//!
//! See [`Selective`](crate::Selective) for the pruned counterpart.

use alloc::sync::Arc;
use core::marker::PhantomData;

use routers_network::{Entry, Metadata, Network};
use routers_trellis::NodeId;

use crate::{
    candidate::Candidate,
    primitives::{PredicateCache, RoutingContext},
    weigh::Weigher,
};

/// Weighs every reachable transition. Inherits the full [`Weigher`] pipeline.
pub struct AllCompute<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    predicate: Arc<PredicateCache<E, M, N>>,
    _phantom: PhantomData<N>,
}

impl<E, M, N> Default for AllCompute<E, M, N>
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

impl<E, M, N> AllCompute<E, M, N>
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

impl<E, M, N> Weigher<E, M, N> for AllCompute<E, M, N>
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
        to_layer: &[Candidate<E>],
    ) -> Vec<NodeId> {
        (0..to_layer.len() as u32).map(NodeId).collect()
    }
}
