/// The identifier for candidates within the [`Candidates`](crate::Candidates) store.
///
/// A plain index newtype: `CandidateId(n)` is the `n`-th candidate inserted during
/// layer generation. Ordering is stable and per-layer sequential, so the layer node
/// vectors (`Layer.nodes`) double as the `(LayerId, NodeId) -> CandidateId` table.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct CandidateId(pub u32);

impl CandidateId {
    /// Construct from a flat insertion index.
    #[inline]
    pub fn new(index: usize) -> Self {
        CandidateId(index as u32)
    }

    /// The flat insertion index of this candidate.
    #[inline]
    pub fn index(&self) -> usize {
        self.0 as usize
    }
}
