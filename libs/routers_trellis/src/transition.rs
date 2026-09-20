#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use crate::trellis::INF_W;

/// Largest resolved transition accepted from the wire. This bounds the dense
/// reconstruction allocation to 4 MiB (`u32` weights) per transition.
#[cfg(feature = "serde")]
pub const MAX_WIRE_TRANSITION_SLOTS: u32 = 1 << 20;

/// Largest number of reachable pairs accepted in one sparse transition.
/// A sparse form cannot contain more useful edges than it has slots.
#[cfg(feature = "serde")]
pub const MAX_WIRE_TRANSITION_EDGES: usize = MAX_WIRE_TRANSITION_SLOTS as usize;

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
pub(super) enum Wire {
    Pending,
    Sparse {
        len: u32,
        #[serde(deserialize_with = "deserialize_bounded_edges")]
        edges: Vec<(u32, u32)>,
    },
}

/// Deserialize a sparse edge list without trusting the wire sequence's size
/// hint. In particular, postcard's count prefix is attacker-controlled.
#[cfg(feature = "serde")]
fn deserialize_bounded_edges<'de, D>(deserializer: D) -> Result<Vec<(u32, u32)>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct BoundedEdges;

    impl<'de> serde::de::Visitor<'de> for BoundedEdges {
        type Value = Vec<(u32, u32)>;

        fn expecting(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            formatter.write_str("a bounded sparse transition edge list")
        }

        fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::SeqAccess<'de>,
        {
            let mut edges = Vec::new();
            while let Some(edge) = sequence.next_element()? {
                if edges.len() == MAX_WIRE_TRANSITION_EDGES {
                    return Err(serde::de::Error::custom(format!(
                        "sparse transition has more than {MAX_WIRE_TRANSITION_EDGES} edges"
                    )));
                }
                edges.push(edge);
            }
            Ok(edges)
        }
    }

    deserializer.deserialize_seq(BoundedEdges)
}

#[cfg(feature = "serde")]
impl Serialize for Transition {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let wire = match self {
            Transition::Pending => Wire::Pending,
            Transition::Resolved(weights) => {
                if weights.len() > MAX_WIRE_TRANSITION_SLOTS as usize {
                    return Err(serde::ser::Error::custom(format!(
                        "transition length {} exceeds wire limit {MAX_WIRE_TRANSITION_SLOTS}",
                        weights.len()
                    )));
                }
                if let Some(&weight) = weights
                    .iter()
                    .find(|&&weight| weight != INF_W && weight > crate::MAX_WEIGHT)
                {
                    return Err(serde::ser::Error::custom(format!(
                        "transition weight {weight} exceeds the maximum"
                    )));
                }
                Wire::Sparse {
                    len: weights.len() as u32,
                    edges: weights
                        .iter()
                        .enumerate()
                        .filter(|&(_, &w)| w != INF_W)
                        .map(|(i, &w)| (i as u32, w))
                        .collect(),
                }
            }
        };
        wire.serialize(serializer)
    }
}

#[cfg(feature = "serde")]
impl<'de> Deserialize<'de> for Transition {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Self::from_wire(Wire::deserialize(deserializer)?)
    }
}

#[cfg(feature = "serde")]
impl Transition {
    pub(super) fn wire_len(wire: &Wire) -> Option<u32> {
        match wire {
            Wire::Pending => None,
            Wire::Sparse { len, .. } => Some(*len),
        }
    }

    pub(super) fn from_wire<E: serde::de::Error>(wire: Wire) -> Result<Self, E> {
        Ok(match wire {
            Wire::Pending => Transition::Pending,
            Wire::Sparse { len, edges } => {
                if len > MAX_WIRE_TRANSITION_SLOTS {
                    return Err(serde::de::Error::custom(format!(
                        "transition length {len} exceeds wire limit {MAX_WIRE_TRANSITION_SLOTS}"
                    )));
                }
                if edges.len() > len as usize {
                    return Err(serde::de::Error::custom(
                        "sparse transition has more edges than slots",
                    ));
                }
                let mut weights = vec![INF_W; len as usize];
                for (index, weight) in edges {
                    let slot = weights.get_mut(index as usize).ok_or_else(|| {
                        serde::de::Error::custom("transition edge index out of range")
                    })?;
                    if weight == INF_W {
                        return Err(serde::de::Error::custom(
                            "sparse transition must not encode an absent edge",
                        ));
                    }
                    if weight > crate::MAX_WEIGHT {
                        return Err(serde::de::Error::custom(
                            "sparse transition edge weight exceeds the maximum",
                        ));
                    }
                    if *slot != INF_W {
                        return Err(serde::de::Error::custom(
                            "sparse transition contains a duplicate edge index",
                        ));
                    }
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

    #[derive(Serialize)]
    #[allow(dead_code)]
    enum MalformedWire {
        Pending,
        Sparse { len: u32, edges: Vec<(u32, u32)> },
    }

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

    #[test]
    fn sparse_wire_rejects_oversized_shape_before_dense_allocation() {
        let oversized = postcard::to_allocvec(&MalformedWire::Sparse {
            len: MAX_WIRE_TRANSITION_SLOTS + 1,
            edges: Vec::new(),
        })
        .unwrap();
        assert!(postcard::from_bytes::<Transition>(&oversized).is_err());

        let impossible_shape = postcard::to_allocvec(&MalformedWire::Sparse {
            len: 1,
            edges: vec![(0, 1), (0, 2)],
        })
        .unwrap();
        assert!(postcard::from_bytes::<Transition>(&impossible_shape).is_err());
    }

    #[test]
    fn sparse_wire_rejects_a_huge_edge_count_prefix_without_allocating_it() {
        // postcard's externally tagged enum form is variant, struct field one,
        // struct field two. The last varint is the edge sequence's count.
        let mut bytes = postcard::to_allocvec(&MalformedWire::Sparse {
            len: 1,
            edges: Vec::new(),
        })
        .unwrap();
        assert_eq!(bytes.pop(), Some(0), "empty sparse edge sequence count");

        let mut count = (MAX_WIRE_TRANSITION_EDGES as u64) + 1;
        while count >= 0x80 {
            bytes.push((count as u8) | 0x80);
            count >>= 7;
        }
        bytes.push(count as u8);
        assert!(postcard::from_bytes::<Transition>(&bytes).is_err());
    }

    #[test]
    fn sparse_wire_rejects_duplicate_indices_and_illegal_weights() {
        let duplicate = postcard::to_allocvec(&MalformedWire::Sparse {
            len: 2,
            edges: vec![(0, 1), (0, 2)],
        })
        .unwrap();
        assert!(postcard::from_bytes::<Transition>(&duplicate).is_err());

        let oversized = Transition::Resolved(vec![INF_W; MAX_WIRE_TRANSITION_SLOTS as usize + 1]);
        assert!(postcard::to_allocvec(&oversized).is_err());

        let illegal_weight = Transition::Resolved(vec![INF_W + 1]);
        assert!(postcard::to_allocvec(&illegal_weight).is_err());

        let illegal_wire = postcard::to_allocvec(&MalformedWire::Sparse {
            len: 1,
            edges: vec![(0, crate::MAX_WEIGHT + 1)],
        })
        .unwrap();
        assert!(postcard::from_bytes::<Transition>(&illegal_wire).is_err());
    }
}
