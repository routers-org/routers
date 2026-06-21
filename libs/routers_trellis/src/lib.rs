//! A layered transition graph (trellis) and solvers for the minimum-cost path.
//!
//! Edges exist only between adjacent layers. Solvers find the minimum-cost
//! path through all layers, treating layer 0 as a virtual start (every node
//! reachable at cost 0) and the last layer as a virtual end (best node wins).
//!
//! # Solvers
//!
//! - [`ViterbiSolver`]: Viterbi with SIMD acceleration.
//! - [`BruteForceSolver`]: Correctness verifier, not reccomended for use.
//!
//! For multi-threaded batch workloads see [`solve_batch`].

mod backend;
mod path;
mod solver;
mod transition;
mod trellis;

pub use path::Path;
pub use solver::{BruteForceSolver, Solve, SolveError, ViterbiSolver};
pub use trellis::{MAX_WEIGHT, NO_EDGE, Trellis, TrellisError};
