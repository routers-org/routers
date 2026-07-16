use crate::{LayerId, Path, Solve, SolveError, Trellis, TrellisError};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

/// A solved trellis: a [`Trellis`] paired with the minimum-cost [`Path`]
/// through it.
///
/// `Solved` is a certificate, not a cache — the only way to construct one is
/// [`Trellis::solve`], and the trellis inside is immutable, so holding a
/// `Solved` guarantees its path describes its trellis. Leaving is consuming:
/// [`append`](Self::append) to grow it, [`reopen`](Self::reopen) to mutate it.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct Solved {
    trellis: Trellis,
    path: Path,
}

impl Solved {
    /// The minimum-cost path through the trellis.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The path's total cost.
    pub fn cost(&self) -> u32 {
        self.path.cost
    }

    /// The solved trellis, read-only.
    pub fn trellis(&self) -> &Trellis {
        &self.trellis
    }

    /// Grow by one layer, returning to the building state with the new
    /// layer's id. The path is discarded — it no longer spans every layer.
    /// A rejected width hands the certificate back untouched.
    // Handing the caller's state back on failure is the point; its size is theirs.
    #[allow(clippy::result_large_err)]
    pub fn append(mut self, width: u32) -> Result<(Trellis, LayerId), (Solved, TrellisError)> {
        match self.trellis.add_layer(width) {
            Ok(id) => Ok((self.trellis, id)),
            Err(e) => Err((self, e)),
        }
    }

    /// Return to the building state for surgery or windowing, discarding
    /// the path.
    pub fn reopen(self) -> Trellis {
        self.trellis
    }
}

impl Trellis {
    /// Solve this trellis into a [`Solved`] certificate; a failed solve hands
    /// the trellis back alongside the error.
    pub fn solve<S: Solve>(self, solver: &S) -> Result<Solved, (Trellis, SolveError)> {
        match solver.solve(&self) {
            Ok(path) => Ok(Solved {
                trellis: self,
                path,
            }),
            Err(e) => Err((self, e)),
        }
    }
}
