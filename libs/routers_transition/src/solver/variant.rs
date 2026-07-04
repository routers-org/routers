use std::sync::Arc;

use crate::{
    CollapsedPath, EmissionStrategy, MatchError, PredicateCache, SelectiveForwardSolver, Solver,
    Transition, TransitionStrategy, TrellisForwardSolver,
};
use routers_network::{Entry, Metadata, Network};

pub enum SolverImpl<E: Entry, M: Metadata, N: Network<E, M>> {
    Selective(SelectiveForwardSolver<E, M, N>),
    Trellis(TrellisForwardSolver<E, M, N>),
}

#[derive(Default, Clone, Debug)]
pub enum SolverVariant {
    /// Fastest available solver. Currently the eager, dense, Viterbi-backed
    /// [`TrellisForwardSolver`].
    #[default]
    Fastest,
    /// Eager solver: materialises the whole transition matrix and solves with a
    /// Viterbi DP on top of `routers_trellis`. (Formerly the petgraph/astar
    /// "precompute" solver; now backed by the trellis.)
    Precompute,
    /// Lazy Upper-Bounded-Dijkstra solver: expands only the frontier it visits.
    Selective,
    /// Explicit alias for the eager trellis solver (same as `Precompute`/`Fastest`).
    Trellis,
}

impl SolverVariant {
    pub(crate) fn without_cache<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
    ) -> SolverImpl<E, M, N> {
        match self {
            SolverVariant::Selective => SolverImpl::Selective(SelectiveForwardSolver::default()),
            // Fastest / Precompute / Trellis all resolve to the eager trellis solver.
            _ => SolverImpl::Trellis(TrellisForwardSolver::default()),
        }
    }

    pub(crate) fn instance<E: Entry, M: Metadata, N: Network<E, M>>(
        self,
        cache: Arc<PredicateCache<E, M, N>>,
    ) -> SolverImpl<E, M, N> {
        match self {
            SolverVariant::Selective => {
                SolverImpl::Selective(SelectiveForwardSolver::default().use_cache(cache))
            }
            _ => SolverImpl::Trellis(TrellisForwardSolver::default().use_cache(cache)),
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
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        match self {
            SolverImpl::Selective(selective) => selective.solve(transition, runtime),
            SolverImpl::Trellis(trellis) => trellis.solve(transition, runtime),
        }
    }
}
