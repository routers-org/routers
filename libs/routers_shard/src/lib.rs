//! Geographic recursive sharding for routing networks.
//!
//! This crate provides traits and concrete strategies for partitioning a
//! routing network into a set of geographically-bounded shards, then
//! constructing a [`ShardedNetwork`] containing only the data relevant
//! to a chosen shard (optionally including its neighbours).
//!
//! Three strategies ship with the crate: [`QuadTreeStrategy`] and
//! [`GeohashStrategy`] subdivide the equirectangular world rectangle, and
//! [`S2Strategy`] (behind the `s2` feature) uses the S2 cell hierarchy on the
//! sphere, which keeps cell areas near-uniform and wraps the antimeridian.
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

#[cfg(feature = "s2")]
pub use strategy::s2::{S2CellId, S2ParseError, S2Strategy};

#[cfg(not(target_arch = "wasm32"))]
pub use loader::FileFetcher;

#[cfg(all(target_arch = "wasm32", feature = "web"))]
pub use loader::WebFetcher;
