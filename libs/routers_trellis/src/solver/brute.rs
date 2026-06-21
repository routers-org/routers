use crate::{
    Path, Solve, SolveError, Trellis,
    trellis::INF_W,
    types::{LayerId, NodeId},
};

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

    fn path_cost(t: &Trellis, nodes: &[usize]) -> u32 {
        let mut cost = 0u32;
        for layer in 0..nodes.len() - 1 {
            let edge = t.edge_weight(
                LayerId(layer as u32),
                NodeId(nodes[layer] as u32),
                NodeId(nodes[layer + 1] as u32),
            );
            cost = cost.saturating_add(edge);
        }
        cost
    }
}

impl Solve for BruteForceSolver {
    fn solve(&mut self, t: &Trellis) -> Result<Path, SolveError> {
        if let Some(layer) = t.first_pending() {
            return Err(SolveError::NotResolved(layer));
        }

        let layers = t.layers();
        let widths = t.widths();

        // Single-layer trellis: no transitions, trivially cost-zero.
        if layers == 1 {
            return Ok(Path::new(vec![NodeId(0)], 0, true));
        }

        let mut best_cost = INF_W;
        let mut best_nodes: Vec<usize> = Vec::new();

        // Enumerate all paths as a multi-digit counter over node indices.
        let mut path = vec![0usize; layers];
        loop {
            let cost = Self::path_cost(t, &path);
            if cost < best_cost {
                best_cost = cost;
                best_nodes = path.clone();
            }

            // Advance to the next combination (increment least-significant digit first).
            let mut carry = true;
            for layer in (0..layers).rev() {
                if carry {
                    path[layer] += 1;
                    if path[layer] < widths[layer] as usize {
                        carry = false;
                    } else {
                        path[layer] = 0;
                    }
                }
            }
            if carry {
                break; // All combinations exhausted.
            }
        }

        Ok(if best_cost < INF_W {
            let nodes = best_nodes.iter().map(|&n| NodeId(n as u32)).collect();
            Path::new(nodes, best_cost, true)
        } else {
            Path::new(Vec::new(), best_cost, false)
        })
    }
}
