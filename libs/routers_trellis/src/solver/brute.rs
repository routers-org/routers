use crate::{Path, Solve, SolveError, Trellis, trellis::INF_W, types::NodeId};

/// Correctness reference: enumerates every possible path and picks the cheapest.
///
/// Time is O(∏ widths × layers) — exponential in the number of layers.
/// Use only for tests or tiny graphs; never in production.
pub struct BruteForceSolver;

impl Default for BruteForceSolver {
    fn default() -> Self {
        BruteForceSolver
    }
}

impl BruteForceSolver {
    pub fn new() -> Self {
        BruteForceSolver
    }
}

impl Solve for BruteForceSolver {
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            level = "debug",
            name = "brute_force",
            skip(self, t),
            fields(layers = t.layers())
        )
    )]
    fn solve(&self, t: &Trellis) -> Result<Path, SolveError> {
        log::warn!(
            "BruteForceSolver: O(∏ widths × layers) — never use in production \
             (layers={}, widths={:?})",
            t.layers(),
            t.widths(),
        );

        if let Some(layer) = t.first_pending() {
            return Err(SolveError::NotResolved(layer));
        }

        let layers = t.layers();
        let widths = t.widths();

        let mut best_cost = INF_W;
        let mut best_nodes: Vec<NodeId> = Vec::new();

        // Enumerate all paths as a multi-digit counter over node indices.
        let mut path = vec![NodeId(0); layers];
        loop {
            let cost = t.path_cost(&path);
            if cost < best_cost {
                best_cost = cost;
                best_nodes = path.clone();
            }

            // Advance to the next combination (increment least-significant digit first).
            let mut carry = true;
            for layer in (0..layers).rev() {
                if carry {
                    path[layer].0 += 1;
                    if path[layer].0 < widths[layer] {
                        carry = false;
                    } else {
                        path[layer] = NodeId(0);
                    }
                }
            }
            if carry {
                break; // All combinations exhausted.
            }
        }

        log::debug!(
            "BruteForceSolver: done — best_cost={best_cost} reachable={}",
            best_cost < INF_W,
        );

        if best_cost >= INF_W {
            return Err(SolveError::Unreachable);
        }

        Ok(Path::new(best_nodes, best_cost))
    }
}
