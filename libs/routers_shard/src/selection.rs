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

    /// Load the owned shard plus the raw nodes/edges that fall within
    /// `padding_distance` metres of its bounds.
    ///
    /// Unlike [`OwnedAndNeighbours`](Self::OwnedAndNeighbours) this does
    /// not pull whole neighbouring shards into memory; it carries a
    /// geometric buffer that the network builder uses to admit individual
    /// graph elements lying just outside the owned cell. The cost is
    /// proportional to the padding distance rather than the shard size,
    /// so the mode scales gracefully as precision drops — at geohash 3
    /// with 50 m of padding the loaded set is essentially the owned
    /// shard plus a thin handover strip, instead of nine continent-sized
    /// cells.
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
    /// When present, the network builder additionally admits any node
    /// whose position lies inside this rectangle, regardless of which
    /// shard it belongs to. Used by
    /// [`SelectionMode::OwnedAndPadded`] to materialise a small strip of
    /// cross-boundary data without loading whole neighbour shards.
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
