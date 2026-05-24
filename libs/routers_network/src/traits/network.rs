//! [`Network`] is the umbrella trait pulling together the data plane and
//! the routing/scan capabilities.
//!
//! Network keeps its `<E, M>` generic-parameter form so existing consumers
//! that bound on `Network<OsmEntryId, OsmEdgeMetadata>` (or similar) keep
//! compiling. The blanket impl below ties `E` / `M` to the data plane's
//! associated types, so any `T: DataPlane + Scan + Route` automatically
//! satisfies `Network<T::Entry, T::Meta>` — no separate impl required.
//!
//! Pick the bound that matches your needs:
//!
//! - `N: DataPlane` — looser; just data access, no routing.
//! - `N: Network<E, M>` — equivalent to `DataPlane + Scan + Route` and
//!   exposes its identity types as the `E` / `M` type parameters.

use core::fmt::Debug;

use crate::{DataPlane, Entry, Metadata, Route, Scan};

// Re-exported so existing callers of `network::GraphEdge` keep working
// after the type moved into the data-plane module.
pub use crate::traits::data_plane::{EdgeData, GraphEdge};

/// Routing-aware network — data plane + nearest-neighbour + shortest path.
///
/// Implemented automatically for every type that satisfies
/// `DataPlane<Entry = E, Meta = M> + Scan<E> + Route<E>`.
pub trait Network<E, M>: DataPlane<Entry = E, Meta = M> + Scan<E> + Route<E> + Debug + Send + Sync
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
