use std::sync::Arc;

use crate::{
    AllComputeSolver, CollapsedPath, EmissionStrategy, MatchError, PredicateCache, SelectiveSolver,
    SideTable, Solver, Transition, TransitionStrategy,
};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::Trellis;

/// A concrete dispatcher over the available [`Solver`] strategies.
///
/// This is what [`SolverVariant`] hands back — a value that is used purely
/// through the [`Solver`] trait, so callers stay decoupled from any specific
/// solver struct.
pub enum SolverImpl<E: Entry, M: Metadata, N: Network<E, M>> {
    AllCompute(AllComputeSolver<E, M, N>),
    Selective(SelectiveSolver<E, M, N>),
}

/// Selects which [`Solver`] strategy a match should use.
#[derive(Default, Clone, Copy, Debug)]
pub enum SolverVariant {
    /// Fastest available solver (the all-compute, fully-parallel weigher).
    #[default]
    Fastest,
    /// Exhaustive all-compute weigher (alias of [`Fastest`](Self::Fastest)).
    Precompute,
    /// Selective (pruned fan-out) weigher — fewer reachability computations.
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

impl<E: Entry, M: Metadata, N: Network<E, M>> Solver<E, M, N> for SolverImpl<E, M, N> {
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
        match self {
            SolverImpl::AllCompute(s) => s.weigh(transition, runtime, trellis, side),
            SolverImpl::Selective(s) => s.weigh(transition, runtime, trellis, side),
        }
    }

    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
        trellis: &mut Trellis,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        match self {
            SolverImpl::AllCompute(s) => s.solve(transition, runtime, trellis),
            SolverImpl::Selective(s) => s.solve(transition, runtime, trellis),
        }
    }
}
