use core::{iter::once, ops::Range};

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

    /// Fill `dist` — the flat DP table positioned by [`Trellis::layer_ranges`]
    /// — with the minimum cost to reach and enter every node from the virtual
    /// source (every first-layer node starts at its own node weight).
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip_all))]
    fn forward_pass(&self, boundaries: &[Boundary], dist: &mut [u32]) {
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
            next_costs.fill(INF_W);

            let reachable = cur_costs
                .iter()
                .zip(boundary.weights.chunks_exact(boundary.next.len()))
                .filter(|&(&cost, _)| cost < INF_W);

            for (&cost, row) in reachable {
                for (next, &edge) in next_costs.iter_mut().zip(row) {
                    *next = (*next).min(cost + edge);
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

    /// Trace the optimal path backwards through `dist`.
    #[cfg_attr(feature = "tracing", tracing::instrument(level = "trace", skip_all))]
    fn backtrack(
        &self,
        last: Range<usize>,
        boundaries: &[Boundary],
        dist: &[u32],
    ) -> Result<Path, SolveError> {
        // Pick the best node in the final layer; ties go to the lowest node.
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

        // Walk the boundaries in reverse, at each picking the predecessor that
        // reaches the chosen node cheapest; ties go to the lowest node. The
        // chosen node's own weight is a shared constant per candidate set, so
        // ignoring it preserves the argmin.
        let tail_to_head: Vec<NodeId> = boundaries
            .iter()
            .rev()
            .scan(final_node, |chosen, boundary| {
                let into_chosen = boundary
                    .weights
                    .iter()
                    .skip(chosen.index())
                    .step_by(boundary.next.len());

                let (_, predecessor) = dist[boundary.cur.clone()]
                    .iter()
                    .zip(into_chosen)
                    .map(|(&cost, &edge)| cost.saturating_add(edge))
                    .enumerate()
                    .map(|(node, cost)| (cost, NodeId::from_index(node)))
                    .min()?;

                *chosen = predecessor;
                Some(predecessor)
            })
            .collect();

        let nodes = tail_to_head
            .into_iter()
            .rev()
            .chain(once(final_node))
            .collect();
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
        let last = t.layer_ranges().last().unwrap_or(0..0);

        // The first layer's starting cost is its node weights; every later
        // layer is overwritten by the forward pass before use.
        let mut dist = t.node_table().to_vec();

        self.forward_pass(&boundaries, &mut dist);
        let path = self.backtrack(last, &boundaries, &dist)?;

        debug!("cost={}", path.cost);
        Ok(path)
    }
}
