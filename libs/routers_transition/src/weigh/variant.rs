use alloc::sync::Arc;

use crate::{AllCompute, Candidate, PredicateCache, RoutingContext, Selective, Weigher};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::NodeId;

/// A [`Weigher`] chosen at runtime by [`SolverVariant`]. Used purely through the
/// [`Weigher`] trait, so callers stay decoupled from any concrete strategy struct.
pub enum WeigherImpl<E: Entry, M: Metadata, N: Network<E, M>> {
    AllCompute(AllCompute<E, M, N>),
    Selective(Selective<E, M, N>),
}

/// Selects which [`Weigher`] strategy a match should use.
#[derive(Default, Clone, Copy, Debug)]
pub enum SolverVariant {
    /// Fastest exact strategy: the fully-parallel all-compute weigher.
    #[default]
    Fastest,
    /// Alias of [`Fastest`](Self::Fastest).
    Precompute,
    /// Selective (pruned fan-out) weigher — fewer reachability computations,
    /// inexact but cheaper on dense candidate sets.
    Selective,
}

impl SolverVariant {
    pub(crate) fn without_cache<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
    ) -> WeigherImpl<E, M, N> {
        match self {
            SolverVariant::Selective => WeigherImpl::Selective(Selective::default()),
            _ => WeigherImpl::AllCompute(AllCompute::default()),
        }
    }

    pub(crate) fn instance<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
        cache: Arc<PredicateCache<E, M, N>>,
    ) -> WeigherImpl<E, M, N> {
        match self {
            SolverVariant::Selective => {
                WeigherImpl::Selective(Selective::default().use_cache(cache))
            }
            _ => WeigherImpl::AllCompute(AllCompute::default().use_cache(cache)),
        }
    }
}

/// Dispatches the two strategy hooks to the chosen weigher; the rest of the
/// pipeline is inherited from [`Weigher`]'s provided methods.
impl<E: Entry, M: Metadata, N: Network<E, M>> Weigher<E, M, N> for WeigherImpl<E, M, N> {
    fn cache(&self) -> &PredicateCache<E, M, N> {
        match self {
            WeigherImpl::AllCompute(weigher) => weigher.cache(),
            WeigherImpl::Selective(weigher) => weigher.cache(),
        }
    }

    fn select(
        &self,
        ctx: &RoutingContext<E, M, N>,
        source: &Candidate<E>,
        to_layer: &[Candidate<E>],
    ) -> Vec<NodeId> {
        match self {
            WeigherImpl::AllCompute(weigher) => weigher.select(ctx, source, to_layer),
            WeigherImpl::Selective(weigher) => weigher.select(ctx, source, to_layer),
        }
    }
}
