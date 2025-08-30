use crate::{
    CollapsedPath, EmissionStrategy, MatchError, PrecomputeForwardSolver, PredicateCache,
    SelectiveForwardSolver, Solver, Transition, TransitionStrategy,
};
use routers_codec::{Entry, Metadata};

pub enum SolverImpl<E: Entry, M: Metadata> {
    Precompute(PrecomputeForwardSolver<E, M>),
    Selective(SelectiveForwardSolver<E, M>),
}

#[derive(Default)]
pub enum SolverVariant {
    #[default]
    Fastest,
    Precompute,
    Selective,
}

impl SolverVariant {
    pub(crate) fn instance<E: Entry, M: Metadata>(
        self,
        cache: PredicateCache<E, M>,
    ) -> SolverImpl<E, M> {
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

impl<E: Entry, M: Metadata> Solver<E, M> for SolverImpl<E, M> {
    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M> + Send + Sync,
    {
        match self {
            SolverImpl::Precompute(precompute) => precompute.solve(transition, runtime),
            SolverImpl::Selective(selective) => selective.solve(transition, runtime),
        }
    }
}
