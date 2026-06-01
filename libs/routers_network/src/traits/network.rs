//! [`Network`] is the umbrella trait pulling together the data plane and
//! the routing/scan capabilities.
//!
//! Pick the bound that matches your needs:
//!
//! - `N: DataPlane` is looser; just data access, no routing.
//! - `N: Network<E, M>` is equivalent to `DataPlane + Scan + Route` with exposed identity types.

use core::fmt::Debug;

use crate::{DataPlane, Entry, Metadata, Route, Scan};

// Re-exported so existing callers of `network::GraphEdge` keep working
// after the type moved into the data-plane module.
pub use crate::traits::data_plane::{EdgeData, GraphEdge};

/// Routing-aware network — data plane + nearest-neighbour + shortest path.
pub trait Network<E, M>:
    DataPlane<Entry = E, Meta = M> + Scan<E> + Route<E> + Debug + Send + Sync
where
    E: Entry,
    M: Metadata,
{
}

impl<T, E, M> Network<E, M> for T
where
    T: DataPlane<Entry = E, Meta = M> + Scan<E> + Route<E> + Debug + Send + Sync,
    E: Entry,
    M: Metadata,
{
}
