use crate::{
    Solve, SolveError,
    backend::Backend,
    path::Path,
    trellis::{INF_W, Trellis},
    types::{LayerId, NodeId},
};

/// Reusable Viterbi solver with SIMD acceleration.
///
/// Owns scratch buffers so repeated solves are allocation-free after warm-up.
/// Create one per worker thread; see `solve_batch` for multi-threaded use.
pub struct ViterbiSolver {
    dist: Vec<u32>,
    offsets: Vec<usize>,
    path: Vec<usize>, // internal scratch; converted to Vec<NodeId> on output
    backend: Backend,
}

impl Default for ViterbiSolver {
    fn default() -> Self {
        Self::new()
    }
}

impl ViterbiSolver {
    pub fn new() -> Self {
        ViterbiSolver {
            dist: Vec::new(),
            offsets: Vec::new(),
            path: Vec::new(),
            backend: Backend::default(),
        }
    }

    /// Fill `self.dist` with the minimum-cost distance to every node.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip_all))]
    fn forward_pass(&mut self, t: &Trellis) {
        // Every first-layer node starts at cost zero (virtual source).
        let first_width = t.widths()[0] as usize;
        for cost in &mut self.dist[..first_width] {
            *cost = 0;
        }

        for layer in 0..t.layers() - 1 {
            let cur_width = t.widths()[layer] as usize;
            let next_width = t.widths()[layer + 1] as usize;
            let cur_start = self.offsets[layer];
            let next_start = self.offsets[layer + 1];
            let weights = t.layer(LayerId(layer as u32)).unwrap(); // safe: all resolved

            log::trace!(
                "forward_pass: layer {}/{} cur_width={cur_width} next_width={next_width}",
                layer,
                t.layers() - 1,
            );

            // split_at_mut lets us hold a mutable `next` slice and an immutable
            // `cur` slice simultaneously without aliasing.
            let (head, tail) = self.dist.split_at_mut(next_start);
            let cur_costs = &head[cur_start..cur_start + cur_width];
            let next_costs = &mut tail[..next_width];

            self.backend
                .dispatch(cur_costs, weights, cur_width, next_width, next_costs);
        }
    }

    /// Trace the optimal path backwards through `self.dist`.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip_all))]
    fn backtrack(&mut self, t: &Trellis) -> Path {
        let last_layer = t.layers() - 1;
        let last_width = t.widths()[last_layer] as usize;
        let last_start = self.offsets[last_layer];

        // Pick the best node in the final layer.
        let mut best_final_node = 0usize;
        let mut best_cost = INF_W;
        for node in 0..last_width {
            let cost = self.dist[last_start + node];
            if cost < best_cost {
                best_cost = cost;
                best_final_node = node;
            }
        }

        log::trace!("backtrack: best_final_node={best_final_node} best_cost={best_cost}");

        if best_cost >= INF_W {
            return Path::new(Vec::new(), best_cost, false);
        }

        self.path[last_layer] = best_final_node;

        // Walk backwards: for each layer find which predecessor leads here cheapest.
        for layer in (0..last_layer).rev() {
            let next_node = self.path[layer + 1];
            let cur_width = t.widths()[layer] as usize;
            let next_width = t.widths()[layer + 1] as usize;
            let cur_start = self.offsets[layer];
            let weights = t.layer(LayerId(layer as u32)).unwrap(); // safe: all resolved

            let mut best_cur_node = 0usize;
            let mut best_candidate = INF_W;
            for node in 0..cur_width {
                let edge_weight = weights[node * next_width + next_node];
                let candidate = self.dist[cur_start + node].saturating_add(edge_weight);
                if candidate < best_candidate {
                    best_candidate = candidate;
                    best_cur_node = node;
                }
            }

            self.path[layer] = best_cur_node;
        }

        let nodes = self.path[..t.layers()]
            .iter()
            .map(|&n| NodeId(n as u32))
            .collect();
        Path::new(nodes, best_cost, true)
    }
}

impl Solve for ViterbiSolver {
    /// Minimum-cost path through `t`. Reuses internal buffers across calls.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(
            level = "debug",
            name = "viterbi",
            skip(self, t),
            fields(layers = t.layers())
        )
    )]
    fn solve(&mut self, t: &Trellis) -> Result<Path, SolveError> {
        log::debug!("ViterbiSolver::solve: {} layers, widths={:?}", t.layers(), t.widths());

        if let Some(layer) = t.first_pending() {
            log::debug!("ViterbiSolver::solve: aborted — L{layer} is pending");
            return Err(SolveError::NotResolved(layer));
        }

        // Build prefix offsets: offsets[l] = index of layer l's first node in `dist`.
        // offsets[layers] = total nodes across all layers.
        self.offsets.clear();
        self.offsets.push(0);
        for &width in t.widths() {
            let next = self.offsets.last().unwrap() + width as usize;
            self.offsets.push(next);
        }
        let total_nodes = *self.offsets.last().unwrap();

        // Grow buffers if this graph is larger than any seen so far.
        if self.dist.len() < total_nodes {
            self.dist.resize(total_nodes, 0);
        }
        if self.path.len() < t.layers() {
            self.path.resize(t.layers(), 0);
        }

        self.forward_pass(t);
        let path = self.backtrack(t);

        log::debug!(
            "ViterbiSolver::solve: done — cost={} reachable={}",
            path.cost,
            path.reachable,
        );
        Ok(path)
    }
}
