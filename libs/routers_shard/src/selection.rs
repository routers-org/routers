//! Selection of shards to be loaded by a single node.
//!
//! A node hosting a routing graph is rarely interested in *exactly* one
//! shard: at the boundary it would have no way to keep routing into the
//! neighbouring node's territory, since transferring a trip mid-route
//! requires the two sides to overlap. The [`Selection`] type packages an
//! owned shard plus a (possibly empty) set of context shards that the node
//! holds purely for handover continuity.

use crate::strategy::{ShardId, ShardingStrategy};
use geo::{Point, Rect, coord};
use rustc_hash::FxHashSet;

/// Describes how to expand an owned shard into a loaded selection.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SelectionMode {
    /// Load only the owned shard.
    Owned,

    /// Load the owned shard plus its immediate cardinal/diagonal neighbours.
    ///
    /// For the quad-tree strategy this is the canonical "9-cell" arrangement
    /// (owned + 8 neighbours).
    OwnedAndNeighbours,

    /// Load the owned shard plus the raw nodes/edges that fall within
    /// `padding_distance` metres of its bounds.
    OwnedAndPadded { padding_distance: f64 },
}

#[derive(Debug, Clone)]
pub struct Selection<S: ShardId> {
    /// The shard that this node has been allocated authority over.
    pub owned: S,
    /// The full set of shards whose data must be present in the local graph,
    /// including `owned`. Look-ups during ingestion use this set.
    pub loaded: FxHashSet<S>,
    /// Optional geographic buffer around the owned shard's bounds.
    ///
    /// Used by [`SelectionMode::OwnedAndPadded`] to obtain a small
    /// strip of cross-boundary data without loading whole neighbour shards.
    ///
    /// As such, it scales more efficiently than [`SelectionMode::OwnedAndNeighbours`]
    /// when the padding distance is constant and shard sizes are large.
    ///
    /// When given enough information about vehicle movement, an entire
    /// shard can be excessive, and have higher memory usage than required.
    pub padding: Option<Rect>,
}

impl<S: ShardId> Selection<S> {
    pub fn new<St>(strategy: &St, owned: S, mode: SelectionMode) -> Self
    where
        St: ShardingStrategy<Id = S>,
    {
        let mut loaded = FxHashSet::default();
        loaded.insert(owned);
        let padding = match mode {
            SelectionMode::Owned => None,
            SelectionMode::OwnedAndNeighbours => {
                for n in strategy.neighbours(&owned) {
                    loaded.insert(n);
                }
                None
            }
            SelectionMode::OwnedAndPadded { padding_distance } => {
                Some(padded_bounds(strategy.bounds(&owned), padding_distance))
            }
        };
        Self {
            owned,
            loaded,
            padding,
        }
    }

    /// Returns `true` if the shard `id` is part of the loaded selection.
    #[inline]
    pub fn contains(&self, id: &S) -> bool {
        self.loaded.contains(id)
    }

    /// Returns `true` if `point` falls within the padded buffer (if any).
    ///
    /// Returns `false` when no padding is configured — selection
    /// membership in that case is decided purely by shard id.
    #[inline]
    pub fn padding_contains(&self, point: Point) -> bool {
        let Some(rect) = self.padding.as_ref() else {
            return false;
        };
        let (x, y) = point.x_y();
        let min = rect.min();
        let max = rect.max();
        x >= min.x && x <= max.x && y >= min.y && y <= max.y
    }
}

/// Expand `rect` by `padding_meters` in both axes, using a local
/// equirectangular conversion centred on the rectangle's midpoint.
fn padded_bounds(rect: Rect, padding_meters: f64) -> Rect {
    const M_PER_DEG: f64 = 111_320.0;
    let cy = 0.5 * (rect.min().y + rect.max().y);
    let pad_y = padding_meters / M_PER_DEG;
    let pad_x = padding_meters / (M_PER_DEG * cy.to_radians().cos().abs().max(1e-6));
    Rect::new(
        coord! { x: rect.min().x - pad_x, y: rect.min().y - pad_y },
        coord! { x: rect.max().x + pad_x, y: rect.max().y + pad_y },
    )
}
