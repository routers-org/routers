//! Selective weigher — weighs only a pruned subset of transitions.
//!
//! [`select`](Weigher::select) keeps, per source, the `fanout` next-layer
//! candidates nearest by straight-line distance, cutting the O(N²) reachability
//! computation per boundary to O(N·fanout). Everything else is inherited from the
//! [`Weigher`] pipeline.
//!
//! # Exactness
//! Proximity pruning is a heuristic: a far-but-optimal target can be missed. For
//! guaranteed-exact matching use [`AllCompute`](crate::AllCompute).

use alloc::sync::Arc;
use core::marker::PhantomData;

use geo::{Distance, Haversine};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::NodeId;

use crate::{
    candidate::Candidate,
    primitives::{PredicateCache, RoutingContext},
    weigh::Weigher,
};

/// Default per-source fan-out: how many nearest next-layer candidates to weigh.
pub const DEFAULT_FANOUT: usize = 16;

/// Weighs the `fanout` nearest next-layer candidates per source. Inherits the
/// full [`Weigher`] pipeline.
pub struct Selective<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    predicate: Arc<PredicateCache<E, M, N>>,
    fanout: usize,
    _phantom: PhantomData<N>,
}

impl<E, M, N> Default for Selective<E, M, N>
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

impl<E, M, N> Selective<E, M, N>
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

impl<E, M, N> Weigher<E, M, N> for Selective<E, M, N>
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
        source: &Candidate<E>,
        to_layer: &[Candidate<E>],
    ) -> Vec<NodeId> {
        let mut nearest = (0..to_layer.len()).collect::<Vec<_>>();
        if nearest.len() > self.fanout {
            let distance_to =
                |i: &usize| Haversine.distance(source.position, to_layer[*i].position);
            // Partial selection: only membership of the nearest `fanout`
            // matters, never their order.
            nearest.select_nth_unstable_by(self.fanout.saturating_sub(1), |a, b| {
                distance_to(a).total_cmp(&distance_to(b))
            });
            nearest.truncate(self.fanout);
        }

        nearest.into_iter().map(|i| NodeId(i as u32)).collect()
    }
}
