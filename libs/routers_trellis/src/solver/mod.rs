use crate::{Path, Trellis, types::LayerId};
use thiserror::Error;

mod brute;
mod viterbi;

pub use brute::BruteForceSolver;
pub use viterbi::ViterbiSolver;

/// A strategy for finding the minimum-cost path through a [`Trellis`].
///
/// Solvers are stateless: any instance may solve any trellis, in any order.
pub trait Solve {
    fn solve(&self, t: &Trellis) -> Result<Path, SolveError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SolveError {
    #[error("transition at layer {0} is not yet resolved")]
    NotResolved(LayerId),
    #[error("no path connects the first layer to the last")]
    Unreachable,
}
