use alloc::sync::Arc;

use crate::{
    AllComputeSolver, Candidate, CandidateId, PredicateCache, RoutingContext, SelectiveSolver,
    Solver,
};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::NodeId;

/// A [`Solver`] chosen at runtime by [`SolverVariant`]. Used purely through the
/// [`Solver`] trait, so callers stay decoupled from any concrete solver struct.
pub enum SolverImpl<E: Entry, M: Metadata, N: Network<E, M>> {
    AllCompute(AllComputeSolver<E, M, N>),
    Selective(SelectiveSolver<E, M, N>),
}

/// Selects which [`Solver`] strategy a match should use.
#[derive(Default, Clone, Copy, Debug)]
pub enum SolverVariant {
    /// Fastest from-scratch solver: the exact, fully-parallel all-compute weigher.
    #[default]
    Fastest,
    /// Alias of [`Fastest`](Self::Fastest).
    Precompute,
    /// Selective (pruned fan-out) weigher — fewer reachability computations, best
    /// for extending a partially-solved trellis.
    Selective,
}

impl SolverVariant {
    pub(crate) fn without_cache<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
    ) -> SolverImpl<E, M, N> {
        match self {
            SolverVariant::Selective => SolverImpl::Selective(SelectiveSolver::default()),
            _ => SolverImpl::AllCompute(AllComputeSolver::default()),
        }
    }

    pub(crate) fn instance<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
        cache: Arc<PredicateCache<E, M, N>>,
    ) -> SolverImpl<E, M, N> {
        match self {
            SolverVariant::Selective => {
                SolverImpl::Selective(SelectiveSolver::default().use_cache(cache))
            }
            _ => SolverImpl::AllCompute(AllComputeSolver::default().use_cache(cache)),
        }
    }
}

/// Dispatches the two strategy hooks to the chosen solver; the rest of the
/// pipeline is inherited from [`Solver`]'s provided methods.
impl<E: Entry, M: Metadata, N: Network<E, M>> Solver<E, M, N> for SolverImpl<E, M, N> {
    fn cache(&self) -> &PredicateCache<E, M, N> {
        match self {
            SolverImpl::AllCompute(solver) => solver.cache(),
            SolverImpl::Selective(solver) => solver.cache(),
        }
    }

    fn select(
        &self,
        ctx: &RoutingContext<E, M, N>,
        source: &Candidate<E>,
        to_layer: &[CandidateId],
    ) -> Vec<NodeId> {
        match self {
            SolverImpl::AllCompute(solver) => solver.select(ctx, source, to_layer),
            SolverImpl::Selective(solver) => solver.select(ctx, source, to_layer),
        }
    }
}
