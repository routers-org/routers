use routers_network::Entry;
use routers_trellis::{LayerId, NodeId};
use serde::{Deserialize, Serialize};

use crate::candidate::{Candidate, CandidateRef};

/// Every candidate considered for a match, layer by layer.
///
/// Layers are aligned one-to-one with the trellis: `layers[l][n]` is the
/// candidate at [`CandidateRef`] `(l, n)`, so lookup is positional and O(1).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct CandidateStore<E>
where
    E: Entry,
{
    layers: Vec<Vec<Candidate<E>>>,
}

impl<E> CandidateStore<E>
where
    E: Entry,
{
    /// Append one layer of candidates, in node order.
    pub(crate) fn push_layer(&mut self, candidates: Vec<Candidate<E>>) {
        self.layers.push(candidates);
    }

    /// Keep only the last `n` layers, re-stamping each surviving candidate's
    /// [`CandidateRef`] so identity stays positional after the shift.
    pub(crate) fn tail(&mut self, n: usize) {
        let cut = self.layers.len().saturating_sub(n);
        if cut == 0 {
            return;
        }

        self.layers.drain(..cut);
        for (layer, candidates) in self.layers.iter_mut().enumerate() {
            for candidate in candidates {
                candidate.location.layer = LayerId(layer as u32);
            }
        }
    }

    /// Keep only `node` in the first layer, re-stamped to `NodeId(0)` — the
    /// candidate counterpart to [`Trellis::pin_first`](routers_trellis::Trellis::pin_first).
    pub(crate) fn pin_first(&mut self, node: NodeId) {
        if let Some(first) = self.layers.first_mut() {
            let mut kept = first[node.index()];
            kept.location = CandidateRef::new(LayerId(0), NodeId(0));
            *first = vec![kept];
        }
    }

    /// The candidates of one layer, in node order.
    pub fn layer(&self, layer: LayerId) -> Option<&[Candidate<E>]> {
        self.layers.get(layer.index()).map(Vec::as_slice)
    }

    /// The [`Candidate`] behind a ref, if present.
    pub fn candidate(&self, r: &CandidateRef) -> Option<Candidate<E>> {
        self.layers
            .get(r.layer.index())?
            .get(r.node.index())
            .copied()
    }

    /// Number of layers stored.
    pub fn layers(&self) -> usize {
        self.layers.len()
    }

    /// Total candidates across all layers.
    pub fn len(&self) -> usize {
        self.layers.iter().map(Vec::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    /// Iterate every candidate, layer-major.
    pub fn iter(&self) -> impl Iterator<Item = &Candidate<E>> {
        self.layers.iter().flatten()
    }
}
