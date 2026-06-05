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
use std::fmt::Display;

/// Identifier for a single shard.
pub trait ShardId:
    Copy + Clone + Eq + Hash + Ord + Debug + Send + Sync + Serialize + DeserializeOwned + 'static
{
}

impl<T> ShardId for T where
    T: Clone
        + Copy
        + Eq
        + Hash
        + Ord
        + Debug
        + Send
        + Sync
        + Serialize
        + DeserializeOwned
        + 'static
{
}

/// A spatial partitioning scheme.
pub trait ShardingStrategy: Debug + Send + Sync {
    type Id: ShardId + Display;

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
