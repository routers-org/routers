#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::trellis::INF_W;

/// The two states of a layer-to-layer transition.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Transition {
    /// Weights/edges to the next layer have not been generated yet.
    Pending,
    /// Fixed weights, row-major `[from * next_width + to]`; absent edges = `INF_W`.
    Resolved(Vec<u32>),
}

impl Transition {
    #[inline]
    pub fn is_resolved(&self) -> bool {
        matches!(self, Transition::Resolved(_))
    }

    #[inline]
    pub fn weights(&self) -> Option<&[u32]> {
        match self {
            Transition::Resolved(w) => Some(w),
            Transition::Pending => None,
        }
    }

    #[inline]
    pub fn weights_mut(&mut self) -> Option<&mut Vec<u32>> {
        match self {
            Transition::Resolved(w) => Some(w),
            Transition::Pending => None,
        }
    }

    /// If `Pending`, initialise to `size` absent edges (`INF_W`) and become `Resolved`.
    pub fn ensure_resolved(&mut self, size: usize) {
        if matches!(self, Transition::Pending) {
            *self = Transition::Resolved(vec![INF_W; size]);
        }
    }
}
