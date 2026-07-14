//! A layered transition graph (trellis) and solvers for the minimum-cost path.
//!
//! Edges exist only between adjacent layers, and every node carries a weight
//! (default 0) paid on entering it. Solvers find the minimum-cost path through
//! all layers, treating layer 0 as a virtual start (every node reachable at
//! its own node weight) and the last layer as a virtual end (best node wins).
//!
//! The trellis stores only *append-stable* facts — structure and weights —
//! and solving is a pure function of them: [`Trellis::solve`] consumes the
//! building state and returns a [`Solved`] certificate pairing the trellis
//! with its path. [`Solved::append`] hands back the building state to grow.
//!
//! # Solvers
//!
//! - [`ViterbiSolver`]: Viterbi with SIMD acceleration. Stateless; usable with any trellis.
//! - [`BruteForceSolver`]: Correctness reference — never use in production.

mod path;
mod solved;
mod solver;
mod transition;
mod trellis;
pub mod types;

pub use path::Path;
pub use solved::Solved;
pub use solver::{BruteForceSolver, Solve, SolveError, ViterbiSolver};
pub use trellis::{MAX_WEIGHT, NO_EDGE, Trellis, TrellisError};
pub use types::{LayerId, NodeId};
