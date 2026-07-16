//! Shared plumbing: errors, the routing context, hop geometry, and the
//! reachability caches.
//!
//! Two of these matter to most users. [`MatchError`] is how every failure
//! surfaces, split by the stage that gave up so you can tell an unroutable
//! trajectory from a lower-level fault. [`PredicateCache`] is the reachability
//! cache a weigher answers routing queries from — share one across matches
//! (see [`MatchOptions::with_cache`](crate::MatchOptions::with_cache)) to keep
//! it warm.
//!
//! The rest is context handed to extension points: [`RoutingContext`] is the
//! read-only view (map, runtime, candidates) that weighers and costing
//! strategies operate against, and [`Reachable`] describes how one candidate
//! is reached from another.

pub(crate) mod algorithms;
mod cache;
mod error;
mod resolve;
mod routing;
mod weight_and_distance;

pub use cache::{PredicateCache, SuccessorsCache};
pub use error::{Disconnected, DisconnectedError, MatchError, Unanchored, UnanchoredError};
pub use resolve::{Reachable, ResolutionMethod};
pub use routing::RoutingContext;
pub use weight_and_distance::WeightAndDistance;

pub(crate) use algorithms::Dijkstra;
