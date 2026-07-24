//! [`Network`] is the umbrella trait pulling together the data plane and
//! the routing/scan capabilities.
//!
//! Pick the bound that matches your needs:
//!
//! - `N: DataPlane` is looser; just data access, no routing.
//! - `N: Network` is equivalent to `DataPlane + Scan + Route`, with the
//!   identity types exposed as associated types (`N::Entry`, `N::Meta`,
//!   `N::Runtime`).

use core::fmt::Debug;

use crate::{DataPlane, Route, Scan};

// Re-exported so existing callers of `network::GraphEdge` keep working
// after the type moved into the data-plane module.
pub use crate::traits::data_plane::{EdgeData, GraphEdge};

/// Routing-aware network — data plane + nearest-neighbour + shortest path.
///
/// The identity types live on [`DataPlane`] as associated types, so a
/// `N: Network` consumer names them as `N::Entry`, `N::Meta` and
/// `N::Runtime`.
pub trait Network: DataPlane + Scan + Route + Debug + Send + Sync {}

impl<T> Network for T where T: DataPlane + Scan + Route + Debug + Send + Sync {}
