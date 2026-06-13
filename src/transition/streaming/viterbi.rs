//! Forward Viterbi primitives shared by the streaming match path.

use crate::transition::candidate::{CandidateEdge, CandidateId};
use crate::transition::layer::definition::Layers;
use rustc_hash::FxHashMap;

/// Per-candidate cumulative best-path costs from a synthetic `start`,
/// computed by a forward Viterbi sweep over a transition-graph DAG.
///
/// Holds a reference to the `Layers` it was computed against so the
/// L_last column can be extracted without re-supplying them.
pub struct ViterbiFrontier<'a> {
    cum_costs: FxHashMap<CandidateId, u32>,
    layers: &'a Layers,
}

impl<'a> ViterbiFrontier<'a> {
    /// Run the forward Viterbi sweep over a `pair` DAG.
    ///
    /// `pair` is the layered DAG produced by the solver's pair
    /// builder: edges run `start → L0 → L1 → … → L_last → end`, each
    /// carrying its `transition + emission` cost. A single in-order
    /// pass converges because by the time we visit layer k, every
    /// candidate in k-1 has its final cum_cost.
    pub fn from_pair(
        pair: &FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>>,
        start: CandidateId,
        layers: &'a Layers,
    ) -> Self {
        let mut cum_costs: FxHashMap<CandidateId, u32> = FxHashMap::default();

        // Seed from start → L0 edges. Each edge weight equals the L0
        // candidate's emission, so this sets `cum[c0] = emission(c0)`.
        if let Some(edges) = pair.get(&start) {
            for (c0, edge) in edges {
                cum_costs.insert(*c0, edge.weight);
            }
        }

        // Walk the user layers in order. Final-layer candidates have
        // outgoing edges to the synthetic `end`; those are populated
        // too but most callers only care about the L_last subset.
        for layer in &layers.layers {
            for &cur in &layer.nodes {
                let Some(cur_cum) = cum_costs.get(&cur).copied() else {
                    continue;
                };
                if let Some(edges) = pair.get(&cur) {
                    for (next, edge) in edges {
                        let candidate = cur_cum.saturating_add(edge.weight);
                        cum_costs
                            .entry(*next)
                            .and_modify(|c| {
                                if candidate < *c {
                                    *c = candidate;
                                }
                            })
                            .or_insert(candidate);
                    }
                }
            }
        }

        Self { cum_costs, layers }
    }

    /// Cumulative cost ending at `candidate`, or `None` if unreachable.
    pub fn cum_cost(&self, candidate: &CandidateId) -> Option<u32> {
        self.cum_costs.get(candidate).copied()
    }

    /// `(CandidateId, cum_cost)` pairs for every reachable candidate
    /// in the final user-layer. Empty if no L_last candidate was
    /// reachable.
    pub fn last_layer(&self) -> Vec<(CandidateId, u32)> {
        self.layers
            .layers
            .last()
            .map(|layer| {
                layer
                    .nodes
                    .iter()
                    .filter_map(|c| self.cum_costs.get(c).map(|&cum| (*c, cum)))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Total reachable candidates across all layers (plus any
    /// synthetic end node, if reached).
    pub fn len(&self) -> usize {
        self.cum_costs.len()
    }

    /// `true` if no candidate is reachable from `start`.
    pub fn is_empty(&self) -> bool {
        self.cum_costs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transition::layer::definition::Layer;
    use geo::Point;

    fn layer(node_ids: &[usize]) -> Layer {
        Layer {
            nodes: node_ids.iter().map(|&i| CandidateId::new(i)).collect(),
            origin: Point::new(0.0, 0.0),
        }
    }

    fn layers_of(slices: &[&[usize]]) -> Layers {
        Layers {
            layers: slices.iter().map(|s| layer(s)).collect(),
        }
    }

    #[test]
    fn seeds_l0_with_emissions() {
        let start = CandidateId::new(0);
        let c0 = CandidateId::new(1);
        let c1 = CandidateId::new(2);
        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        pair.insert(
            start,
            vec![(c0, CandidateEdge::new(7)), (c1, CandidateEdge::new(11))],
        );
        let layers = layers_of(&[&[1, 2]]);

        let frontier = ViterbiFrontier::from_pair(&pair, start, &layers);
        assert_eq!(frontier.cum_cost(&c0), Some(7));
        assert_eq!(frontier.cum_cost(&c1), Some(11));
    }

    #[test]
    fn picks_min_predecessor() {
        //   start →(1)→ a →(50)→ x
        //         →(100)→ b →(2)→ x
        // cum[x] = min(1+50, 100+2) = 51.
        let start = CandidateId::new(0);
        let a = CandidateId::new(1);
        let b = CandidateId::new(2);
        let x = CandidateId::new(3);

        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        pair.insert(
            start,
            vec![(a, CandidateEdge::new(1)), (b, CandidateEdge::new(100))],
        );
        pair.insert(a, vec![(x, CandidateEdge::new(50))]);
        pair.insert(b, vec![(x, CandidateEdge::new(2))]);

        let layers = layers_of(&[&[1, 2], &[3]]);
        let frontier = ViterbiFrontier::from_pair(&pair, start, &layers);
        assert_eq!(frontier.cum_cost(&x), Some(51));
    }

    #[test]
    fn empty_when_start_has_no_edges() {
        let start = CandidateId::new(0);
        let pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        let layers = layers_of(&[&[1, 2]]);
        let frontier = ViterbiFrontier::from_pair(&pair, start, &layers);
        assert!(frontier.is_empty());
        assert!(frontier.last_layer().is_empty());
    }

    #[test]
    fn omits_unreachable_candidates() {
        let start = CandidateId::new(0);
        let a = CandidateId::new(1);
        let b = CandidateId::new(2);
        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        pair.insert(start, vec![(a, CandidateEdge::new(5))]);

        let layers = layers_of(&[&[1, 2]]);
        let frontier = ViterbiFrontier::from_pair(&pair, start, &layers);
        assert_eq!(frontier.cum_cost(&a), Some(5));
        assert_eq!(frontier.cum_cost(&b), None);
    }

    #[test]
    fn three_layer_linear_chain() {
        //   start →(2)→ a →(3)→ b →(7)→ c
        // cum[c] = 12.
        let start = CandidateId::new(0);
        let a = CandidateId::new(1);
        let b = CandidateId::new(2);
        let c = CandidateId::new(3);

        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        pair.insert(start, vec![(a, CandidateEdge::new(2))]);
        pair.insert(a, vec![(b, CandidateEdge::new(3))]);
        pair.insert(b, vec![(c, CandidateEdge::new(7))]);

        let layers = layers_of(&[&[1], &[2], &[3]]);
        let frontier = ViterbiFrontier::from_pair(&pair, start, &layers);
        assert_eq!(frontier.cum_cost(&c), Some(12));
    }

    #[test]
    fn last_layer_returns_only_terminal_candidates() {
        //   start →(2)→ a →(3)→ b
        // L0={a}, L1={b}. last_layer should be just b.
        let start = CandidateId::new(0);
        let a = CandidateId::new(1);
        let b = CandidateId::new(2);

        let mut pair: FxHashMap<CandidateId, Vec<(CandidateId, CandidateEdge)>> =
            FxHashMap::default();
        pair.insert(start, vec![(a, CandidateEdge::new(2))]);
        pair.insert(a, vec![(b, CandidateEdge::new(3))]);

        let layers = layers_of(&[&[1], &[2]]);
        let frontier = ViterbiFrontier::from_pair(&pair, start, &layers);
        let last = frontier.last_layer();
        assert_eq!(last, vec![(b, 5)]);
    }
}
