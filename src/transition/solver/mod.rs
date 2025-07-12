#[doc(hidden)]
pub mod fast_layer_sweep;
#[doc(hidden)]
pub mod methods;
#[doc(hidden)]
pub mod precompute_forward;
#[doc(hidden)]
pub mod selective_forward;

#[doc(inline)]
pub use fast_layer_sweep::*;
#[doc(inline)]
pub use methods::*;
#[doc(inline)]
pub use precompute_forward::*;
#[doc(inline)]
pub use selective_forward::*;
use std::sync::{Arc, Mutex};

use crate::{
    CollapsedPath, EmissionStrategy, MatchError, PredicateCache, Transition, TransitionStrategy,
};
use routers_codec::{Entry, Metadata};

pub enum SolverImpl<E: Entry, M: Metadata> {
    Fast(FastLayerSweepSolver<E, M>),
    Precompute(PrecomputeForwardSolver<E, M>),
    Selective(SelectiveForwardSolver<E, M>),
}

pub enum SolverVariant {
    Fast,
    Precompute,
    Selective,
}

impl SolverVariant {
    pub(crate) fn instance<E: Entry, M: Metadata>(
        self,
        cache: Arc<Mutex<PredicateCache<E, M>>>,
    ) -> SolverImpl<E, M> {
        match self {
            SolverVariant::Fast => {
                // TODO: Give it a cache
                SolverImpl::Fast(FastLayerSweepSolver::default())
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
            SolverImpl::Fast(fast) => fast.solve(transition, runtime),
            SolverImpl::Precompute(precompute) => precompute.solve(transition, runtime),
            SolverImpl::Selective(selective) => selective.solve(transition, runtime),
        }
    }
}
