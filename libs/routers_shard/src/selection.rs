//! Selection of shards to be loaded by a single node.
//!
//! A node hosting a routing graph is rarely interested in *exactly* one
//! shard: at the boundary it would have no way to keep routing into the
//! neighbouring node's territory, since transferring a trip mid-route
//! requires the two sides to overlap. The [`Selection`] type packages an
//! owned shard plus a (possibly empty) set of context shards that the node
//! holds purely for handover continuity.

use crate::strategy::{ShardId, ShardingStrategy};
use rustc_hash::FxHashSet;

/// Describes how to expand an owned shard into a loaded selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionMode {
    /// Load only the owned shard.
    ///
    /// Cheapest, but trips that approach a shard boundary lose connectivity
    /// before the orchestrator can hand them off.
    Owned,

    /// Load the owned shard plus its immediate cardinal/diagonal neighbours.
    ///
    /// For the quad-tree strategy this is the canonical "9-cell" arrangement
    /// (owned + 8 neighbours). The owned shard is the only one for which the
    /// node accepts new traffic; the rest exist purely to keep edges
    /// resolvable across the boundary.
    OwnedAndNeighbours,
}

#[derive(Debug, Clone)]
pub struct Selection<S: ShardId> {
    /// The shard that this node has been allocated authority over.
    pub owned: S,
    /// The full set of shards whose data must be present in the local graph,
    /// including `owned`. Look-ups during ingestion use this set.
    pub loaded: FxHashSet<S>,
}

impl<S: ShardId> Selection<S> {
    pub fn new<St>(strategy: &St, owned: S, mode: SelectionMode) -> Self
    where
        St: ShardingStrategy<Id = S>,
    {
        let mut loaded = FxHashSet::default();
        loaded.insert(owned.clone());
        if matches!(mode, SelectionMode::OwnedAndNeighbours) {
            for n in strategy.neighbours(&owned) {
                loaded.insert(n);
            }
        }
        Self { owned, loaded }
    }

    /// Returns `true` if the shard `id` is part of the loaded selection.
    #[inline]
    pub fn contains(&self, id: &S) -> bool {
        self.loaded.contains(id)
    }
}
