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

/// Run one [`ViterbiSolver`] per slot (thread), distributing `trellises` evenly.
///
/// Each result mirrors what a single-threaded `ViterbiSolver::solve` would return,
/// including `Err(SolveError::NotResolved(_))` for any unresolved trellis.
pub fn solve_batch(trellises: &[Trellis], slots: usize) -> Vec<Result<Path, SolveError>> {
    let n = trellises.len();
    let mut out: Vec<Result<Path, SolveError>> = (0..n)
        .map(|_| Ok(Path::new(Vec::new(), 0, false)))
        .collect();

    if n == 0 {
        return out;
    }

    let slots = slots.max(1).min(n);
    let chunk = n.div_ceil(slots);

    std::thread::scope(|s| {
        for (tin, tout) in trellises.chunks(chunk).zip(out.chunks_mut(chunk)) {
            s.spawn(move || {
                let mut solver = ViterbiSolver::new();
                for (k, t) in tin.iter().enumerate() {
                    tout[k] = solver.solve(t);
                }
            });
        }
    });

    out
}
