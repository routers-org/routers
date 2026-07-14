use crate::{Candidate, CandidateRef};
use routers_network::Entry;
use routers_trellis::LayerId;
use serde::{Deserialize, Serialize};

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
