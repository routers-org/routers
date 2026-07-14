use routers_trellis::{LayerId, NodeId};
use serde::{Deserialize, Serialize};

/// The positional identity of a candidate: which layer it anchors, and which
/// node of that layer it is.
///
/// Identity *is* placement — a candidate's ref equals its trellis coordinates,
/// so no separate id space (or the bookkeeping to keep one aligned) exists.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct CandidateRef {
    pub layer: LayerId,
    pub node: NodeId,
}

impl CandidateRef {
    pub fn new(layer: LayerId, node: NodeId) -> Self {
        Self { layer, node }
    }
}

impl From<(LayerId, NodeId)> for CandidateRef {
    fn from((layer, node): (LayerId, NodeId)) -> Self {
        Self { layer, node }
    }
}

impl core::fmt::Display for CandidateRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "L{}N{}", self.layer, self.node)
    }
}
