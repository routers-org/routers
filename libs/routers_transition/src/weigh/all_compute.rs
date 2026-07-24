//! All-compute weigher weighs *every* reachable transition, hence the name.
//!
//! See [`Selective`](crate::Selective) for the pruned counterpart.

use alloc::sync::Arc;

use routers_network::Network;
use routers_trellis::NodeId;

use crate::{
    candidate::Candidate,
    primitives::{PredicateCache, RoutingContext},
    weigh::Weigher,
};

/// Weighs every reachable transition. Inherits the full [`Weigher`] pipeline.
pub struct AllCompute<N>
where
    N: Network,
{
    predicate: Arc<PredicateCache<N>>,
}

impl<N> Default for AllCompute<N>
where
    N: Network,
{
    fn default() -> Self {
        Self {
            predicate: Arc::new(PredicateCache::default()),
        }
    }
}

impl<N> AllCompute<N>
where
    N: Network,
{
    pub fn use_cache(self, cache: Arc<PredicateCache<N>>) -> Self {
        Self {
            predicate: cache,
            ..self
        }
    }
}

impl<N> Weigher<N> for AllCompute<N>
where
    N: Network,
{
    fn cache(&self) -> &PredicateCache<N> {
        &self.predicate
    }

    fn select(
        &self,
        _ctx: &RoutingContext<N>,
        _source: &Candidate<N::Entry>,
        to_layer: &[Candidate<N::Entry>],
    ) -> Vec<NodeId> {
        (0..to_layer.len() as u32).map(NodeId).collect()
    }
}
