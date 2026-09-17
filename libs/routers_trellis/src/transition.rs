#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::trellis::INF_W;

/// The two states of a layer-to-layer transition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Transition {
    /// Weights/edges to the next layer have not been generated yet.
    Pending,
    /// Fixed weights, row-major `[from * next_width + to]`; absent edges = `INF_W`.
    Resolved(Vec<u32>),
}

/// The wire form: only reachable pairs travel, since most of a dense matrix is `INF_W`.
#[cfg(feature = "serde")]
#[derive(Serialize, Deserialize)]
enum Wire {
    Pending,
    Sparse { len: u32, edges: Vec<(u32, u32)> },
}

#[cfg(feature = "serde")]
impl Serialize for Transition {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Transition::Pending => Wire::Pending,
            Transition::Resolved(weights) => Wire::Sparse {
                len: weights.len() as u32,
                edges: weights
                    .iter()
                    .enumerate()
                    .filter(|&(_, &w)| w != INF_W)
                    .map(|(i, &w)| (i as u32, w))
                    .collect(),
            },
        };
        wire.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for Transition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(match Wire::deserialize(deserializer)? {
            Wire::Pending => Transition::Pending,
            Wire::Sparse { len, edges } => {
                let mut weights = vec![INF_W; len as usize];
                for (index, weight) in edges {
                    let slot = weights.get_mut(index as usize).ok_or_else(|| {
                        serde::de::Error::custom("transition edge index out of range")
                    })?;
                    *slot = weight;
                }
                Transition::Resolved(weights)
            }
        })
    }
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

#[cfg(all(test, feature = "serde"))]
mod tests {
    use super::*;

    #[test]
    fn resolved_round_trips_through_the_sparse_wire_form() {
        let mut weights = vec![INF_W; 12];
        weights[1] = 7;
        weights[11] = 30_000;
        let original = Transition::Resolved(weights);
        let bytes = postcard::to_allocvec(&original).unwrap();
        assert!(
            bytes.len() < 12 * 5,
            "sparse form must beat one varint per slot"
        );
        assert_eq!(
            postcard::from_bytes::<Transition>(&bytes).unwrap(),
            original
        );
        let pending = postcard::to_allocvec(&Transition::Pending).unwrap();
        assert_eq!(
            postcard::from_bytes::<Transition>(&pending).unwrap(),
            Transition::Pending
        );
    }
}
