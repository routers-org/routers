//! Geographic recursive sharding for routing networks.
//!
//! This crate provides traits and concrete strategies for partitioning a
//! routing network into a set of geographically-bounded shards, then
//! constructing a [`ShardedNetwork`] containing only the data relevant
//! to a chosen shard (optionally including its neighbours).
//!
//! The library is agnostic of the underlying map data format: it operates
//! on the generic [`Entry`](routers_network::Entry) and
//! [`Metadata`](routers_network::Metadata) traits. An OSM-specific ingestion
//! adapter is provided behind the `osm` feature for convenience.

pub mod filter;
pub mod loader;
pub mod network;
pub mod selection;
pub mod source;
pub mod strategy;

pub use filter::IngestFilter;
pub use loader::{LoadError, RecenterDelta, ShardCache, ShardFetcher, ShardLoader, ShardWindow};
pub use network::ShardedNetwork;
pub use selection::{Selection, SelectionMode};
pub use source::{NodeRecord, ShardSource, WayRecord};
pub use strategy::{
    ShardId, ShardingStrategy,
    geohash::{Geohash, GeohashStrategy},
    quadtree::{QuadKey, QuadTreeStrategy},
};

#[cfg(not(target_arch = "wasm32"))]
pub use loader::FileShardFetcher;

#[cfg(target_arch = "wasm32")]
pub use loader::WebShardFetcher;

#[cfg(feature = "osm")]
pub mod osm;
