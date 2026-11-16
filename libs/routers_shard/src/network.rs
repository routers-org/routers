//! Generic sharded routing network.
//!
//! [`ShardedNetwork`] mirrors the structure of `OsmNetwork` but is generic
//! over the entry and metadata types, so the same builder can drop in any
//! data source that satisfies [`ShardSource`](crate::ShardSource).

use core::fmt::Debug;
use geo::Point;
use log::{debug, info};
use rustc_hash::{FxHashMap, FxHashSet};
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
// `web_time::Instant` is a drop-in for `std::time::Instant` that doesn't
// panic on `wasm32-unknown-unknown` (`std::time` has no clock source there).
use web_time::Instant;

use routers_network::{
    Entry, Metadata, Node,
    columnar::{self, Graph, RawEdge, blob, unblob},
    columnar_network,
    network::{EdgeData, GraphEdge},
};

use crate::selection::Selection;
use crate::strategy::{ShardId, ShardingStrategy};

/// A data source from which a [`ShardedNetwork`] can be built.
///
/// Implement this trait on any type that provides an iterable collection of
/// nodes (id + position) and directed edges (from, to, weight and edge id,
/// metadata). Edges that share an id must share metadata.
/// [`ShardedNetwork::from_source`] then filters and assembles these into the
/// sharded graph structure.
pub trait ShardSource<E: Entry, M: Metadata> {
    fn nodes<'a>(&'a self) -> Box<dyn Iterator<Item = (E, Point)> + 'a>;
    fn edges<'a>(&'a self) -> Box<dyn Iterator<Item = SourceEdge<E, M>> + 'a>;
}

/// One directed edge yielded by a [`ShardSource`]: from, to, weight and edge
/// id, metadata.
pub type SourceEdge<E, M> = (E, E, EdgeData<E>, M);

/// Magic header + schema fingerprint prepended to every shard cache file.
/// `CACHE_VERSION` is derived in `build.rs` from the serialised type
/// declarations, so only wire-incompatible revisions invalidate a cache.
const CACHE_MAGIC: &[u8; 4] = b"SHRD";

include!(concat!(env!("OUT_DIR"), "/format_hash.rs"));

/// Bumped when a serialised field changes meaning but not type, which the
/// format hash cannot see. 1: `meta` is keyed by edge id, not source node.
/// 2: the graph is held as columns.
const FORMAT_REVISION: u64 = 2;
const CACHE_VERSION: u64 = FORMAT_HASH ^ FORMAT_REVISION ^ columnar::FORMAT;

/// A routing network restricted to a single shard selection.
///
/// The type parameters are:
/// - `E`: the [`Entry`] type identifying nodes and ways
/// - `M`: the [`Metadata`] attached to each way
/// - `S`: the [`ShardId`] type produced by the strategy used at build time.
///   The selection metadata is retained on the network so that consumers
///   can ask which shard a given node falls in without re-running the
///   strategy.
pub struct ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    graph: Graph<E, M>,

    /// The shard this node has authority over.
    pub owned: S,
    /// All shards whose data is materialised in the graph.
    pub loaded: FxHashSet<S>,
}

columnar_network!(impl[E: Entry, M: Metadata, S: ShardId] ShardedNetwork<E, M, S> => graph: Graph<E, M>);

impl<E, M, S> ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    pub fn num_nodes(&self) -> usize {
        self.graph.num_nodes()
    }

    pub fn num_edges(&self) -> usize {
        self.graph.num_edges()
    }

    /// Every node of the shard.
    pub fn nodes(&self) -> impl Iterator<Item = &Node<E>> {
        self.graph.nodes()
    }

    /// Every directed edge of the shard, grouped by the node it leaves.
    pub fn edges(&self) -> impl Iterator<Item = GraphEdge<E>> + '_ {
        self.graph.edges()
    }

    /// Every way of the shard with its metadata.
    pub fn ways(&self) -> impl Iterator<Item = (E, &M)> {
        self.graph.ways()
    }

    /// The columns the shard is held as.
    pub fn graph(&self) -> &Graph<E, M> {
        &self.graph
    }

    /// A shard over a graph already cut to it.
    pub(crate) fn over(graph: Graph<E, M>, owned: S, loaded: FxHashSet<S>) -> Self {
        Self {
            graph,
            owned,
            loaded,
        }
    }

    /// A shard over parts already filtered to it, without spatial indices.
    pub(crate) fn from_parts(
        nodes: Vec<Node<E>>,
        edges: Vec<RawEdge<E>>,
        ways: Vec<(E, M)>,
        owned: S,
        loaded: FxHashSet<S>,
    ) -> Self {
        Self {
            graph: Graph::assemble(nodes, edges, ways),
            owned,
            loaded,
        }
    }

    /// Build a [`ShardedNetwork`] from a generic [`ShardSource`].
    ///
    /// Only nodes whose shard (as determined by `strategy.locate`) appears in
    /// `selection.loaded` are included. Edges are included if both endpoints
    /// are present in the filtered node set.
    pub fn from_source<Src, St>(
        source: &Src,
        strategy: &St,
        selection: &Selection<S>,
    ) -> Result<Self, String>
    where
        Src: ShardSource<E, M>,
        St: ShardingStrategy<Id = S>,
    {
        let mut hash: FxHashMap<E, Node<E>> = FxHashMap::default();
        let mut edges = Vec::new();
        let mut ways = Vec::new();

        let all_nodes: FxHashMap<E, Point> = source.nodes().collect();

        // Consider a node, if (either or..):
        // 1. It's shard is in the loaded set, or
        // 2. It's position falls within the optional padded buffer.
        for (&id, &pos) in &all_nodes {
            let admit = selection.padding_contains(pos) || {
                let shard = strategy.locate(pos);
                selection.contains(&shard)
            };
            if admit {
                hash.insert(id, Node::new(pos, id));
            }
        }

        for (from, to, (weight, id), m) in source.edges() {
            if !hash.contains_key(&from) {
                continue;
            }

            if !hash.contains_key(&to) {
                if let Some(&pos) = all_nodes.get(&to) {
                    hash.insert(to, Node::new(pos, to));
                } else {
                    continue;
                }
            }

            edges.push(RawEdge {
                source: from,
                target: to,
                class: weight,
                way: id,
            });
            ways.push((id.index(), m));
        }

        let mut net = Self::from_parts(
            hash.into_values().collect(),
            edges,
            ways,
            selection.owned,
            selection.loaded.clone(),
        );

        net.rebuild_indices();
        Ok(net)
    }

    /// Rebuild the spatial indices over the graph.
    pub fn rebuild_indices(&mut self) {
        self.graph.index();
    }

    /// Encode `self` into a `Vec<u8>` with the format header prepended.
    ///
    /// The spatial indices are intentionally not encoded; they are rebuilt
    /// by [`from_cached_bytes`](Self::from_cached_bytes) on load. This is
    /// the WASM-friendly counterpart to
    /// [`save_to_file`](Self::save_to_file).
    pub fn to_cache_bytes(&self) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        out.extend_from_slice(CACHE_MAGIC);
        out.extend_from_slice(&CACHE_VERSION.to_le_bytes());

        blob(&mut out, &self.owned)?;
        blob(&mut out, &self.loaded)?;
        self.graph.encode(&mut out)?;
        Ok(out)
    }

    /// Decode a sharded network from a byte slice produced by
    /// [`to_cache_bytes`](Self::to_cache_bytes), then rebuild the spatial
    /// indices. Filesystem-free — suitable for WASM consumers fetching the
    /// blob over HTTP.
    pub fn from_cached_bytes(bytes: &[u8]) -> Result<Self, String>
    where
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned,
        S: serde::de::DeserializeOwned,
    {
        const HEADER_LEN: usize = CACHE_MAGIC.len() + 8;
        if bytes.len() < HEADER_LEN {
            return Err("fingerprint hash missing".to_string());
        }

        if bytes.len() < HEADER_LEN || &bytes[..CACHE_MAGIC.len()] != CACHE_MAGIC {
            return Err(
                "mismatching fingerprint, cache was generated using a different schema".to_string(),
            );
        }

        let version = u64::from_le_bytes(
            bytes[CACHE_MAGIC.len()..HEADER_LEN]
                .try_into()
                .expect("8 bytes"),
        );
        if version != CACHE_VERSION {
            return Err(format!(
                "shard cache bytes have format hash {version:016x}, expected {CACHE_VERSION:016x}. The shard layout has changed — rebuild the cache."
            ));
        }
        let deser_start = Instant::now();
        let mut payload = &bytes[HEADER_LEN..];
        let mut net = Self {
            owned: unblob(&mut payload)?,
            loaded: unblob(&mut payload)?,
            graph: Graph::decode(&mut payload)?,
        };
        let deser = deser_start.elapsed();

        let rebuild_start = Instant::now();
        net.rebuild_indices();
        let rebuild = rebuild_start.elapsed();
        info!(
            "ShardedNetwork::from_cached_bytes {} bytes — decode {:?}, rebuild {:?}",
            bytes.len(),
            deser,
            rebuild
        );
        Ok(net)
    }

    /// Persist this network to a `.shard.rt` file on disk. Thin wrapper
    /// around [`to_cache_bytes`](Self::to_cache_bytes); not available on
    /// WASM targets.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        let started = Instant::now();
        let bytes = self.to_cache_bytes()?;
        let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        debug!(
            "ShardedNetwork::save_to_file wrote {} bytes (incl. 12-byte header, format {:016x}) in {:?}",
            bytes.len(),
            CACHE_VERSION,
            started.elapsed()
        );
        Ok(())
    }

    /// Read a saved `.shard.rt` from disk. Thin wrapper around
    /// [`from_cached_bytes`](Self::from_cached_bytes); not available on
    /// WASM targets.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_cached(path: &Path) -> Result<Self, String>
    where
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned,
        S: serde::de::DeserializeOwned,
    {
        let read_start = Instant::now();
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        debug!(
            "ShardedNetwork::from_cached read {} bytes in {:?}",
            bytes.len(),
            read_start.elapsed()
        );
        Self::from_cached_bytes(&bytes)
            .map_err(|e| format!("shard cache `{}`: {e}", path.display()))
    }
}

impl<E, M, S> Debug for ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ShardedNetwork(owned={:?}, loaded={}, nodes={}, edges={})",
            self.owned,
            self.loaded.len(),
            self.num_nodes(),
            self.num_edges(),
        )
    }
}
