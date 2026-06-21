use crate::{Path, Trellis};
use thiserror::Error;

mod brute;
mod viterbi;

pub use brute::BruteForceSolver;
pub use viterbi::ViterbiSolver;

/// A strategy for finding the minimum-cost path through a [`Trellis`].
pub trait Solve {
    fn solve(&mut self, t: &Trellis) -> Result<Path, SolveError>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SolveError {
    #[error("transition at layer {0} is not yet resolved")]
    NotResolved(usize),
}
