use core::ops::Range;

use log::{debug, trace, warn};

use crate::{
    Solve, SolveError,
    path::Path,
    trellis::{INF_W, Trellis},
    types::{LayerId, NodeId},
};

/// Viterbi solver with SIMD acceleration.
///
/// Stateless: all working memory is scoped to a single [`Solve::solve`] call,
/// so any solver instance may be used with any trellis, in any order.
/// (Benchmarks showed buffer reuse across solves saves well under 3% even on
/// small trellises — the DP dominates the allocations.)
#[derive(Debug, Default, Clone, Copy)]
pub struct ViterbiSolver;

/// One layer-to-layer boundary, resolved and positioned: the transition
/// weights, the entered layer's node weights, and both adjacent layers' node
/// ranges within the flat DP table.
struct Boundary<'a> {
    id: LayerId,
    cur: Range<usize>,
    next: Range<usize>,
    weights: &'a [u32],
    node_weights: &'a [u32],
}

impl ViterbiSolver {
    pub fn new() -> Self {
        ViterbiSolver
    }

    /// The coalescence layer: the most recent layer whose single node every
    /// finite frontier node descends from. The optimal path's prefix up to
    /// this layer is immutable under any appended layers, so a streaming
    /// consumer may commit it (see `COALESCENCE.md` for the proof). The anchor
    /// node itself is the solved path's node at this layer.
    ///
    /// `None` when the surviving paths never merge — sustained ambiguity, where
    /// nothing is safe to commit — or when the trellis does not solve.
    pub fn coalescence(&self, t: &Trellis) -> Option<LayerId> {
        let boundaries = Self::boundaries(t).ok()?;
        let ranges: Vec<Range<usize>> = t.layer_ranges().collect();

        let mut dist = t.node_table().to_vec();
        let mut back = vec![NodeId(0); dist.len()];
        self.forward_pass(&boundaries, &mut dist, &mut back);

        // Start from every finite node of the last layer and walk `back`. The
        // set can only shrink, so the first layer at which it holds one node is
        // the deepest (most recent) shared ancestor — the coalescence layer.
        let last = ranges.last()?;
        let mut ancestors: Vec<NodeId> = dist[last.clone()]
            .iter()
            .enumerate()
            .filter(|&(_, &cost)| cost < INF_W)
            .map(|(node, _)| NodeId::from_index(node))
            .collect();
        if ancestors.is_empty() {
            return None; // unreachable: no path to anchor
        }

        for (layer, range) in ranges.iter().enumerate().rev() {
            if ancestors.len() == 1 {
                return Some(LayerId(layer as u32));
            }
            if layer == 0 {
                break;
            }
            for node in &mut ancestors {
                *node = back[range.start + node.index()];
            }
            ancestors.sort_unstable();
            ancestors.dedup();
        }
        None
    }

    /// Every boundary of `t`, or the first unresolved one as the error —
    /// succeeding doubles as the proof that the trellis is solvable.
    fn boundaries(t: &Trellis) -> Result<Vec<Boundary<'_>>, SolveError> {
        let ranges: Vec<Range<usize>> = t.layer_ranges().collect();
        t.boundaries()
            .zip(ranges.iter().zip(ranges.iter().skip(1)))
            .map(|(id, (cur, next))| {
                t.layer(id)
                    .map(|weights| Boundary {
                        id,
                        cur: cur.clone(),
                        next: next.clone(),
                        weights,
                        node_weights: &t.node_table()[next.clone()],
                    })
                    .ok_or(SolveError::NotResolved(id))
            })
            .collect()
    }

    /// Fill `dist` and `back` — flat tables positioned by
    /// [`Trellis::layer_ranges`]. `dist[v]` becomes the minimum cost to reach
    /// and enter node `v` from the virtual source (every first-layer node
    /// starts at its own node weight); `back[v]` becomes the predecessor on
    /// that cheapest path, i.e. the argmin the forward pass already computes.
    /// First-layer `back` entries are left untouched — those nodes have no
    /// predecessor.
    ///
    /// Ties resolve to the lowest [`NodeId`]: predecessors are scanned in node
    /// order and only a strict improvement overwrites, so the first (lowest)
    /// one to reach a node's minimum wins. A chosen node's own weight is added
    /// after the argmin, so it never perturbs it.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip_all))]
    fn forward_pass(&self, boundaries: &[Boundary], dist: &mut [u32], back: &mut [NodeId]) {
        for boundary in boundaries {
            trace!(
                "boundary {}: cur_width={} next_width={}",
                boundary.id,
                boundary.cur.len(),
                boundary.next.len(),
            );

            // split_at_mut lets us hold a mutable `next` slice and an immutable
            // `cur` slice simultaneously without aliasing.
            let (head, tail) = dist.split_at_mut(boundary.next.start);
            let cur_costs = &head[boundary.cur.clone()];
            let next_costs = &mut tail[..boundary.next.len()];
            let next_back = &mut back[boundary.next.clone()];
            next_costs.fill(INF_W);

            let reachable = cur_costs
                .iter()
                .zip(boundary.weights.chunks_exact(boundary.next.len()))
                .enumerate()
                .filter(|&(_, (&cost, _))| cost < INF_W);

            for (from, (&cost, row)) in reachable {
                for ((next, into), &edge) in next_costs.iter_mut().zip(&mut *next_back).zip(row) {
                    let candidate = cost + edge;
                    if candidate < *next {
                        *next = candidate;
                        *into = NodeId::from_index(from);
                    }
                }
            }

            // Entering a node costs its weight, paid once per node.
            for (next, &weight) in next_costs.iter_mut().zip(boundary.node_weights) {
                if *next < INF_W {
                    *next += weight;
                }
            }
        }
    }

    /// Trace the optimal path backwards by following `back` from the cheapest
    /// final node (ties to the lowest node).
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip_all))]
    fn backtrack(
        &self,
        ranges: &[Range<usize>],
        dist: &[u32],
        back: &[NodeId],
    ) -> Result<Path, SolveError> {
        let last = ranges.last().cloned().unwrap_or(0..0);
        let Some((best_cost, final_node)) = dist[last]
            .iter()
            .enumerate()
            .map(|(node, &cost)| (cost, NodeId::from_index(node)))
            .min()
        else {
            return Err(SolveError::Unreachable);
        };

        trace!("final_node={final_node} best_cost={best_cost}");

        if best_cost >= INF_W {
            return Err(SolveError::Unreachable);
        }

        // Walk parent pointers from the final node; `back` is layer-local, so
        // index it through each layer's range.
        let mut nodes = vec![NodeId(0); ranges.len()];
        let mut node = final_node;
        for (layer, range) in ranges.iter().enumerate().rev() {
            nodes[layer] = node;
            if layer > 0 {
                node = back[range.start + node.index()];
            }
        }
        Ok(Path::new(nodes, best_cost))
    }
}

impl Solve for ViterbiSolver {
    /// Minimum-cost path through `t`.
    #[cfg_attr(
        feature = "tracing",
        tracing::instrument(level = "debug", name = "viterbi", skip(self, t), fields(layers = t.layers()))
    )]
    fn solve(&self, t: &Trellis) -> Result<Path, SolveError> {
        debug!("{} layers, widths={:?}", t.layers(), t.widths());

        let boundaries = Self::boundaries(t).inspect_err(|e| warn!("{e}"))?;
        let ranges: Vec<Range<usize>> = t.layer_ranges().collect();

        // The first layer's starting cost is its node weights; every later
        // layer is overwritten by the forward pass before use.
        let mut dist = t.node_table().to_vec();
        let mut back = vec![NodeId(0); dist.len()];

        self.forward_pass(&boundaries, &mut dist, &mut back);
        let path = self.backtrack(&ranges, &dist, &back)?;

        debug!("cost={}", path.cost);
        Ok(path)
    }
}
