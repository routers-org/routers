//! Sharding strategy abstractions.
//!
//! A [`ShardingStrategy`] is a deterministic function from a geographic point
//! to a [`ShardId`], plus the geometric metadata needed to enumerate
//! neighbours and to test containment.

pub mod geohash;
pub mod quadtree;

use core::fmt::Debug;
use core::hash::Hash;
use geo::{Point, Rect};
use serde::{Serialize, de::DeserializeOwned};

/// Identifier for a single shard.
///
/// IDs must be cheap to compare and hash so that selections can be assembled
/// into the standard hashed containers. They must also serialise so that an
/// upstream orchestrator can hand them to a worker process.
pub trait ShardId:
    Clone + Eq + Hash + Ord + Debug + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

impl<T> ShardId for T where
    T: Clone + Eq + Hash + Ord + Debug + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

/// A spatial partitioning scheme.
///
/// Strategies are stateless configuration objects (e.g. "quad-tree of depth
/// 12"). They expose:
///
/// 1. [`locate`](Self::locate): point → shard
/// 2. [`bounds`](Self::bounds): shard → bounding box (used during ingestion)
/// 3. [`neighbours`](Self::neighbours): shard → adjacent shards (used to
///    build padded selections)
/// 4. [`contains`](Self::contains): a convenience predicate for a point being
///    inside a shard, implemented in terms of `bounds` by default.
pub trait ShardingStrategy: Debug + Send + Sync {
    type Id: ShardId;

    fn locate(&self, point: Point) -> Self::Id;

    fn bounds(&self, id: &Self::Id) -> Rect;

    fn neighbours(&self, id: &Self::Id) -> Vec<Self::Id>;

    #[inline]
    fn contains(&self, id: &Self::Id, point: Point) -> bool {
        let rect = self.bounds(id);
        let (x, y) = point.x_y();
        let min = rect.min();
        let max = rect.max();
        x >= min.x && x <= max.x && y >= min.y && y <= max.y
    }
}
