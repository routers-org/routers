use std::sync::Arc;

use crate::{
    CollapsedPath, EmissionStrategy, MatchError, PrecomputeForwardSolver, PredicateCache,
    SelectiveForwardSolver, Solver, Transition, TransitionStrategy,
};
use routers_network::{Entry, Metadata, Network};

pub enum SolverImpl<E: Entry, M: Metadata, N: Network<E, M>> {
    Precompute(PrecomputeForwardSolver<E, M, N>),
    Selective(SelectiveForwardSolver<E, M, N>),
}

#[derive(Default, Clone)]
pub enum SolverVariant {
    #[default]
    Fastest,
    Precompute,
    Selective,
}

impl SolverVariant {
    pub(crate) fn without_cache<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
    ) -> SolverImpl<E, M, N> {
        match self {
            SolverVariant::Fastest => SolverImpl::Precompute(PrecomputeForwardSolver::default()),
            SolverVariant::Precompute => SolverImpl::Precompute(PrecomputeForwardSolver::default()),
            SolverVariant::Selective => SolverImpl::Selective(SelectiveForwardSolver::default()),
        }
    }

    pub(crate) fn instance<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
        cache: Arc<PredicateCache<E, M, N>>,
    ) -> SolverImpl<E, M, N> {
        match self {
            SolverVariant::Fastest => {
                SolverImpl::Precompute(PrecomputeForwardSolver::default().use_cache(cache))
            }
            SolverVariant::Precompute => {
                SolverImpl::Precompute(PrecomputeForwardSolver::default().use_cache(cache))
            }
            SolverVariant::Selective => {
                SolverImpl::Selective(SelectiveForwardSolver::default().use_cache(cache))
            }
        }
    }
}

impl<E: Entry, M: Metadata, N: Network<E, M>> Solver<E, M, N> for SolverImpl<E, M, N> {
    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync,
    {
        match self {
            SolverImpl::Precompute(precompute) => precompute.solve(transition, runtime),
            SolverImpl::Selective(selective) => selective.solve(transition, runtime),
        }
    }
}
