//! Selective weigher — weighs only a pruned subset of transitions.
//!
//! # Exactness
//! Proximity pruning is a heuristic: a far-but-optimal target can be missed. For
//! guaranteed-exact matching use [`AllCompute`](crate::AllCompute).

use alloc::sync::Arc;

use geo::{Distance, Haversine};
use routers_network::Network;
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
pub struct Selective<N>
where
    N: Network,
{
    predicate: Arc<PredicateCache<N>>,
    fanout: usize,
}

impl<N> Default for Selective<N>
where
    N: Network,
{
    fn default() -> Self {
        Self {
            predicate: Arc::new(PredicateCache::default()),
            fanout: DEFAULT_FANOUT,
        }
    }
}

impl<N> Selective<N>
where
    N: Network,
{
    /// Weigh against `cache`, adopting its `reach_distance`. To weigh at a
    /// different reach, pass a cache built by
    /// `PredicateCache::with_reach_distance`.
    pub fn use_cache(self, cache: Arc<PredicateCache<N>>) -> Self {
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

impl<N> Weigher<N> for Selective<N>
where
    N: Network,
{
    fn cache(&self) -> &PredicateCache<N> {
        &self.predicate
    }

    fn select(
        &self,
        _ctx: &RoutingContext<N>,
        source: &Candidate<N::Entry>,
        to_layer: &[Candidate<N::Entry>],
    ) -> Vec<NodeId> {
        if to_layer.len() <= self.fanout {
            return (0..to_layer.len() as u32).map(NodeId).collect();
        }

        // Measure each target once.
        let mut nearest = to_layer
            .iter()
            .enumerate()
            .map(|(i, target)| (Haversine.distance(source.position, target.position), i))
            .collect::<Vec<_>>();

        // Only membership of the nearest `fanout` matters, not their order.
        nearest.select_nth_unstable_by(self.fanout.saturating_sub(1), |a, b| a.0.total_cmp(&b.0));
        nearest.truncate(self.fanout);

        nearest.into_iter().map(|(_, i)| NodeId(i as u32)).collect()
    }
}
