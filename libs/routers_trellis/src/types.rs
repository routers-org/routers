use core::fmt;

/// Index of a layer in a [`Trellis`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LayerId(pub u32);

/// Index of a node within a single layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

impl LayerId {
    /// This id as a `usize`, for indexing parallel per-layer collections.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

impl NodeId {
    /// This id as a `usize`, for indexing parallel per-node collections.
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }

    /// Inverse of [`index`](NodeId::index): the id for a position in a
    /// parallel per-node collection.
    #[inline]
    pub fn from_index(index: usize) -> Self {
        NodeId(index as u32)
    }
}

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Display for NodeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}
