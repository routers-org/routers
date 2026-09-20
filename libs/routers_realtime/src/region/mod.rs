//! Region catalog and resolver.

pub mod catalog;
pub mod resolver;

pub use catalog::{Catalog, CatalogError, LaneCount, Region, ReplicaRangeError, Replicas};
pub use resolver::{Pin, Resolution, ResolutionKind, Resolver, Unserved};
