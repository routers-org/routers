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

pub mod composite;
pub mod loader;
pub mod network;
pub mod selection;
pub mod strategy;

pub use composite::MultiShardNetwork;
pub use loader::{Fetcher, LoadError, ShardCache, ShardLoader, ShardMoveDelta, ShardWindow};
pub use network::{ShardSource, ShardedNetwork};
pub use selection::{Selection, SelectionMode};
pub use strategy::{
    ShardId, ShardingStrategy,
    geohash::{Geohash, GeohashParseError, GeohashStrategy},
    quadtree::{QuadKey, QuadTreeStrategy},
};

#[cfg(not(target_arch = "wasm32"))]
pub use loader::FileFetcher;

#[cfg(target_arch = "wasm32")]
pub use loader::WebFetcher;
