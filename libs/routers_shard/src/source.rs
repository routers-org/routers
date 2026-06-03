//! Format-agnostic ingestion API.
//!
//! The shard crate must not depend on any particular wire format (OSM PBF,
//! GeoJSON, vector tiles, ...). All it needs is a way to walk over nodes —
//! geographic points keyed by an [`Entry`](routers_network::Entry) — and
//! ways — sequences of node references with a piece of
//! [`Metadata`](routers_network::Metadata).
//!
//! A concrete adapter implements [`ShardSource`] and the shard builder
//! takes it from there. The OSM adapter (behind the `osm` feature) is one
//! such implementation; user-defined sources slot in identically.

use geo::Point;
use routers_network::{Entry, Metadata};

/// A node observed during the first ingestion pass.
#[derive(Debug, Clone, Copy)]
pub struct NodeRecord<E: Entry> {
    pub id: E,
    pub position: Point,
}

/// A way observed during the second ingestion pass.
///
/// `weight` is computed by the source — typically from `metadata` — so that
/// the shard crate doesn't need to know how a given format derives travel
/// cost.
#[derive(Debug, Clone)]
pub struct WayRecord<E: Entry, M: Metadata> {
    pub id: E,
    pub refs: Vec<E>,
    pub metadata: M,
    pub weight: routers_network::edge::Weight,
    pub bidirectional: bool,
}

/// A two-pass ingestion source.
///
/// The two-pass split is required because deciding whether a way belongs to
/// a shard needs the positions of its referenced nodes, and most map
/// formats expose nodes and ways in a single linear stream. The
/// implementation may stream the underlying file twice or buffer it once.
pub trait ShardSource {
    type Entry: Entry;
    type Metadata: Metadata;
    type Error: core::fmt::Debug;

    fn for_each_node<F>(&self, f: F) -> Result<(), Self::Error>
    where
        F: FnMut(NodeRecord<Self::Entry>) + Send;

    fn for_each_way<F>(&self, f: F) -> Result<(), Self::Error>
    where
        F: FnMut(WayRecord<Self::Entry, Self::Metadata>) + Send;
}
