//! A layered transition graph (trellis) and solvers for the minimum-cost path.
//!
//! Edges exist only between adjacent layers. Solvers find the minimum-cost
//! path through all layers, treating layer 0 as a virtual start (every node
//! reachable at cost 0) and the last layer as a virtual end (best node wins).
//!
//! # Solvers
//!
//! - [`ViterbiSolver`]: Viterbi with SIMD acceleration. Stateless; usable with any trellis.
//! - [`BruteForceSolver`]: Correctness reference — never use in production.

mod path;
mod solver;
mod transition;
mod trellis;
pub mod types;

pub use path::Path;
pub use solver::{BruteForceSolver, Solve, SolveError, ViterbiSolver};
pub use trellis::{MAX_WEIGHT, NO_EDGE, Trellis, TrellisError};
pub use types::{LayerId, NodeId};
