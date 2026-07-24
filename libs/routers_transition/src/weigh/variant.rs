use alloc::sync::Arc;

use routers_network::Network;
use routers_trellis::NodeId;

use crate::{
    candidate::Candidate,
    primitives::{PredicateCache, RoutingContext},
    weigh::{AllCompute, Selective, Weigher},
};

/// A [`Weigher`] chosen at runtime by [`SolverVariant`]. Used purely through the
/// [`Weigher`] trait, so callers stay decoupled from any concrete strategy struct.
pub enum WeigherImpl<N: Network> {
    AllCompute(AllCompute<N>),
    Selective(Selective<N>),
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
    pub(crate) fn without_cache<N: Network>(
        self,
    ) -> WeigherImpl<N> {
        match self {
            SolverVariant::Selective => WeigherImpl::Selective(Selective::default()),
            _ => WeigherImpl::AllCompute(AllCompute::default()),
        }
    }

    pub(crate) fn instance<N: Network>(
        self,
        cache: Arc<PredicateCache<N>>,
    ) -> WeigherImpl<N> {
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
impl<N: Network> Weigher<N> for WeigherImpl<N> {
    fn cache(&self) -> &PredicateCache<N> {
        match self {
            WeigherImpl::AllCompute(weigher) => weigher.cache(),
            WeigherImpl::Selective(weigher) => weigher.cache(),
        }
    }

    fn select(
        &self,
        ctx: &RoutingContext<N>,
        source: &Candidate<N::Entry>,
        to_layer: &[Candidate<N::Entry>],
    ) -> Vec<NodeId> {
        match self {
            WeigherImpl::AllCompute(weigher) => weigher.select(ctx, source, to_layer),
            WeigherImpl::Selective(weigher) => weigher.select(ctx, source, to_layer),
        }
    }
}
