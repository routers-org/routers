//! Generic sharded routing network.
//!
//! [`ShardedNetwork`] mirrors the structure of `OsmNetwork` but is generic
//! over the entry and metadata types, so the same builder can drop in any
//! data source that satisfies [`ShardSource`](crate::ShardSource).

use core::fmt::Debug;
use core::hash::BuildHasherDefault;
use geo::Point;
use log::{debug, info};
use petgraph::prelude::DiGraphMap;
use rstar::{AABB, RTree};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};
use serde::{Deserialize, Serialize};
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::Path;
use std::time::Instant;

use routers_network::{
    DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Node, Route, Scan,
    edge::Weight, network::GraphEdge,
};

use crate::{
    IngestFilter, ShardSource,
    selection::Selection,
    strategy::{ShardId, ShardingStrategy},
};

pub type GraphStructure<E> =
    DiGraphMap<E, (Weight, DirectionAwareEdgeId<E>), BuildHasherDefault<FxHasher>>;

/// Magic header + format fingerprint prepended to every shard cache file.
///
/// `CACHE_VERSION` is computed at build time (see `build.rs`) from the
/// source files that contribute to the serialised layout — change one and
/// the hash rotates automatically, so older cache files are rejected with
/// a useful error instead of a cryptic `postcard` varint panic.
const CACHE_MAGIC: &[u8; 4] = b"SHRD";
include!(concat!(env!("OUT_DIR"), "/format_hash.rs"));
const CACHE_VERSION: u64 = FORMAT_HASH;

/// A routing network restricted to a single shard selection.
///
/// The type parameters are:
/// - `E`: the [`Entry`] type identifying nodes and ways
/// - `M`: the [`Metadata`] attached to each way
/// - `S`: the [`ShardId`] type produced by the strategy used at build time.
///   The selection metadata is retained on the network so that consumers
///   can ask which shard a given node falls in without re-running the
///   strategy.
#[derive(Serialize, Deserialize)]
#[serde(bound(
    serialize = "E: Serialize, M: Serialize, S: Serialize",
    deserialize = "E: serde::de::DeserializeOwned, M: serde::de::DeserializeOwned, S: serde::de::DeserializeOwned"
))]
pub struct ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    pub graph: GraphStructure<E>,
    pub hash: FxHashMap<E, Node<E>>,
    pub meta: FxHashMap<E, M>,

    #[serde(skip)]
    pub index: RTree<Node<E>>,
    #[serde(skip)]
    pub index_edge: RTree<Edge<Node<E>>>,

    /// The shard this node has authority over.
    pub owned: S,
    /// All shards whose data is materialised in `graph`.
    pub loaded: FxHashSet<S>,
}

impl<E, M, S> ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    /// Build a sharded network from a [`ShardSource`] by filtering against
    /// the supplied [`Selection`].
    ///
    /// The build runs in two passes:
    ///
    /// 1. **Node pass.** Every node's position is recorded; nodes whose
    ///    position falls in a loaded shard are flagged as "in-selection".
    /// 2. **Way pass.** A way is kept iff at least one of its referenced
    ///    nodes is in-selection. Kept ways pull in *all* of their
    ///    referenced nodes, including those just outside the selection —
    ///    this gives a one-edge halo around the loaded shards so that
    ///    boundary edges remain resolvable on both ends.
    pub fn from_source<Src, St>(
        source: &Src,
        strategy: &St,
        selection: &Selection<S>,
    ) -> Result<Self, Src::Error>
    where
        Src: ShardSource<Entry = E, Metadata = M>,
        St: ShardingStrategy<Id = S>,
        M: 'static,
    {
        Self::from_source_filtered(source, strategy, selection, &IngestFilter::new())
    }

    /// Build like [`from_source`](Self::from_source) but consult `filter`
    /// per-way to drop ways and/or omit their metadata. Filtering at this
    /// layer (rather than post-build) means dropped data never reaches
    /// the indices or the cache.
    pub fn from_source_filtered<Src, St>(
        source: &Src,
        strategy: &St,
        selection: &Selection<S>,
        filter: &IngestFilter<M>,
    ) -> Result<Self, Src::Error>
    where
        Src: ShardSource<Entry = E, Metadata = M>,
        St: ShardingStrategy<Id = S>,
        M: 'static,
    {
        let fixed_start = Instant::now();
        let mut start = Instant::now();

        // Pass 1: node positions and per-shard membership.
        let mut positions: FxHashMap<E, Point> = FxHashMap::default();
        let mut in_selection: FxHashSet<E> = FxHashSet::default();
        source.for_each_node(|n| {
            let shard = strategy.locate(n.position);
            if selection.contains(&shard) {
                in_selection.insert(n.id);
            }
            positions.insert(n.id, n.position);
        })?;
        debug!(
            "Sharded ingest pass 1 (nodes): {} positions, {} in-selection, {:?}",
            positions.len(),
            in_selection.len(),
            start.elapsed()
        );
        start = Instant::now();

        // Pass 2: ways that touch the selection and pass the user filter.
        let mut kept_ways: Vec<(E, M, Vec<E>, Weight, bool)> = Vec::new();
        let mut needed: FxHashSet<E> = FxHashSet::default();
        let mut dropped_by_filter: usize = 0;
        source.for_each_way(|way| {
            let touches = way.refs.iter().any(|r| in_selection.contains(r));
            if !touches {
                return;
            }
            if !filter.accepts(&way.metadata) {
                dropped_by_filter += 1;
                return;
            }
            for r in &way.refs {
                needed.insert(*r);
            }
            kept_ways.push((
                way.id,
                way.metadata,
                way.refs,
                way.weight,
                way.bidirectional,
            ));
        })?;
        if dropped_by_filter > 0 {
            debug!(
                "Sharded ingest filter dropped {dropped_by_filter} ways before graph construction"
            );
        }
        debug!(
            "Sharded ingest pass 2 (ways): {} kept, {} referenced nodes, {:?}",
            kept_ways.len(),
            needed.len(),
            start.elapsed()
        );
        start = Instant::now();

        // Materialise the graph, the node hash, and the edge list.
        let mut graph = GraphStructure::new();
        let mut meta: FxHashMap<E, M> = FxHashMap::default();
        let mut edges: Vec<Edge<E>> = Vec::new();
        let retain_metadata = filter.keep_metadata();
        for (way_id, metadata, refs, weight, bidi) in kept_ways {
            if retain_metadata {
                meta.insert(way_id, metadata);
            }
            for window in refs.windows(2) {
                let (a, b) = (window[0], window[1]);
                let dir = DirectionAwareEdgeId::new(way_id);
                graph.add_edge(a, b, (weight, dir.forward()));
                edges.push(Edge {
                    source: a,
                    target: b,
                    weight,
                    id: dir.forward(),
                });
                if bidi {
                    graph.add_edge(b, a, (weight, dir.backward()));
                    edges.push(Edge {
                        source: b,
                        target: a,
                        weight,
                        id: dir.backward(),
                    });
                }
            }
        }

        let mut hash: FxHashMap<E, Node<E>> = FxHashMap::default();
        for id in &needed {
            if let Some(pos) = positions.get(id) {
                hash.insert(*id, Node::new(*pos, *id));
            }
        }
        // Drop nodes that don't actually appear in the graph (e.g. way refs
        // pointing at IDs we never saw in the node pass — happens at the
        // edge of clipped extracts).
        hash.retain(|id, _| graph.contains_node(*id));

        let fat: Vec<Edge<Node<E>>> = edges
            .iter()
            .filter_map(|edge| {
                Some(Edge {
                    source: *hash.get(&edge.source)?,
                    target: *hash.get(&edge.target)?,
                    id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), edge.id.index()))
                        .with_direction(edge.id.direction()),
                    weight: edge.weight,
                })
            })
            .collect();
        debug!("Sharded ingest graph build: {:?}", start.elapsed());
        start = Instant::now();

        let index = RTree::bulk_load(hash.values().copied().collect());
        let index_edge = RTree::bulk_load(fat);
        debug!("Sharded ingest rtree build: {:?}", start.elapsed());

        info!(
            "Sharded ingest finished: {} nodes, {} edges, owned={:?}, loaded={}, total {}ms",
            hash.len(),
            graph.edge_count(),
            selection.owned,
            selection.loaded.len(),
            fixed_start.elapsed().as_millis()
        );

        Ok(Self {
            graph,
            hash,
            meta,
            index,
            index_edge,
            owned: selection.owned.clone(),
            loaded: selection.loaded.clone(),
        })
    }

    pub fn num_nodes(&self) -> usize {
        self.graph.node_count()
    }

    /// Rebuild the spatial indices (`index` and `index_edge`) from the
    /// `hash` and `graph` fields.
    ///
    /// The indices are intentionally not serialised — bulk-loading an
    /// `RTree` from N items is O(N log N) and runs at hundreds of MB/s in
    /// practice, which is faster than letting `postcard` decode a tree
    /// structure that takes proportionally more bytes on disk. Call this
    /// after deserialising a network in custom code paths;
    /// [`from_cached`](Self::from_cached) does it for you.
    pub fn rebuild_indices(&mut self) {
        // Bulk-loading the two `RTree`s is the dominant cost on cache hits
        // (~350ms for a 9-shard Sydney load). The two trees are independent
        // so we farm them out to `rayon::join` and pay one tree's worth of
        // wall-clock time instead of the sum.
        let nodes: Vec<Node<E>> = self.hash.values().copied().collect();
        let edges: Vec<Edge<Node<E>>> = self
            .graph
            .all_edges()
            .filter_map(|(s, t, &(weight, id))| {
                let source = *self.hash.get(&s)?;
                let target = *self.hash.get(&t)?;
                Some(Edge {
                    source,
                    target,
                    id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), id.index()))
                        .with_direction(id.direction()),
                    weight,
                })
            })
            .collect();
        let (node_index, edge_index) =
            rayon::join(|| RTree::bulk_load(nodes), || RTree::bulk_load(edges));
        self.index = node_index;
        self.index_edge = edge_index;
    }

    /// Encode `self` into a `Vec<u8>` with the format header prepended.
    ///
    /// The spatial indices are intentionally not encoded; they are rebuilt
    /// by [`from_cached_bytes`](Self::from_cached_bytes) on load. This is
    /// the WASM-friendly counterpart to
    /// [`save_to_file`](Self::save_to_file).
    pub fn to_cache_bytes(&self) -> Result<Vec<u8>, String> {
        let payload = postcard::to_allocvec(self)
            .map_err(|e| format!("failed to serialise sharded network: {e}"))?;
        let mut out = Vec::with_capacity(CACHE_MAGIC.len() + 8 + payload.len());
        out.extend_from_slice(CACHE_MAGIC);
        out.extend_from_slice(&CACHE_VERSION.to_le_bytes());
        out.extend_from_slice(&payload);
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
        if bytes.len() < HEADER_LEN || &bytes[..CACHE_MAGIC.len()] != CACHE_MAGIC {
            return Err(
                "shard cache bytes are missing the SHRD magic header — likely from a previous format. Rebuild the cache.".to_string()
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
        let mut net: Self = postcard::from_bytes(&bytes[HEADER_LEN..])
            .map_err(|e| format!("failed to deserialise sharded network: {e}"))?;
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

    /// Build from a source if no cache exists at `cache_path`, otherwise
    /// load from disk. The classic "expensive build → fast subsequent
    /// loads" idiom, mirroring `OsmNetwork::from_pbf_and_save`.
    ///
    /// Filesystem-bound; not available on WASM. Browser consumers should
    /// fetch the cache bytes themselves and call
    /// [`from_cached_bytes`](Self::from_cached_bytes) directly.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_source_or_cache<Src, St>(
        source: &Src,
        strategy: &St,
        selection: &crate::selection::Selection<S>,
        cache_path: &Path,
    ) -> Result<Self, String>
    where
        Src: ShardSource<Entry = E, Metadata = M>,
        St: ShardingStrategy<Id = S>,
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned + 'static,
        S: serde::de::DeserializeOwned,
    {
        Self::from_source_or_cache_filtered(
            source,
            strategy,
            selection,
            &IngestFilter::new(),
            cache_path,
        )
    }

    /// Filtered cousin of [`from_source_or_cache`](Self::from_source_or_cache).
    ///
    /// **Important**: cache files are keyed only by `cache_path`. If you
    /// reuse a path with a different filter you'll load the previously
    /// filtered network and the new predicate will be silently ignored.
    /// Encode the filter into the path (e.g. `sydney_tertiary+_d10.rt`)
    /// when you mix multiple filters in the same workspace.
    ///
    /// Filesystem-bound; not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_source_or_cache_filtered<Src, St>(
        source: &Src,
        strategy: &St,
        selection: &crate::selection::Selection<S>,
        filter: &IngestFilter<M>,
        cache_path: &Path,
    ) -> Result<Self, String>
    where
        Src: ShardSource<Entry = E, Metadata = M>,
        St: ShardingStrategy<Id = S>,
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned + 'static,
        S: serde::de::DeserializeOwned,
    {
        // Self-heal on a stale cache: if the format hash baked into the
        // file doesn't match the one this binary was built with, the
        // layout on disk is no longer compatible — silently rebuild.
        if cache_path.exists() {
            match Self::from_cached(cache_path) {
                Ok(net) => return Ok(net),
                Err(e) => {
                    log::warn!(
                        "shard cache `{}` unusable ({e}); rebuilding",
                        cache_path.display()
                    );
                }
            }
        }
        let net = Self::from_source_filtered(source, strategy, selection, filter)
            .map_err(|e| format!("ingest failed: {e:?}"))?;
        if let Some(parent) = cache_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        net.save_to_file(cache_path)?;
        Ok(net)
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
            self.graph.node_count(),
            self.graph.edge_count(),
        )
    }
}

impl<E, M, S> Discovery<E> for ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Edge<Node<E>>> + Send + 'a>
    where
        E: 'a,
    {
        Box::new(self.index_edge.locate_in_envelope_intersecting(&aabb))
    }

    fn nodes_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a>
    where
        E: 'a,
    {
        Box::new(self.index.locate_in_envelope(&aabb))
    }

    fn node(&self, id: &E) -> Option<&Node<E>> {
        self.hash.get(id)
    }

    fn edge(&self, source: &E, target: &E) -> Option<Edge<E>> {
        self.graph
            .edge_weight(*source, *target)
            .map(|&(weight, id)| Edge {
                source: *source,
                target: *target,
                weight,
                id,
            })
    }
}

impl<E, M, S> Scan<E> for ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<E>>
    where
        E: 'a,
    {
        self.index.nearest_neighbor(point)
    }
}

impl<E, M, S> Route<E> for ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn route_nodes(&self, start: E, finish: E) -> Option<(Weight, Vec<Node<E>>)> {
        let (score, path) = petgraph::algo::astar(
            &self.graph,
            start,
            |n| n == finish,
            |(_, _, w)| w.0,
            |_| 0 as Weight,
        )?;
        let route = path
            .iter()
            .filter_map(|v| self.hash.get(v).copied())
            .collect();
        Some((score, route))
    }
}

impl<E, M, S> routers_network::DataPlane for ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    type Entry = E;
    type Meta = M;

    fn metadata(&self, id: &E) -> Option<&M> {
        self.meta.get(id)
    }

    fn point(&self, id: &E) -> Option<Point> {
        self.hash.get(id).map(|v| v.position)
    }

    fn edges_into<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Incoming)
                .map(|(s, t, &d)| (s, t, d)),
        )
    }

    fn edges_outof<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Outgoing)
                .map(|(s, t, &d)| (s, t, d)),
        )
    }

    fn fatten(&self, edge: &Edge<E>) -> Option<Edge<Node<E>>> {
        Some(Edge {
            source: *self.hash.get(&edge.source)?,
            target: *self.hash.get(&edge.target)?,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), edge.id.index())),
            weight: edge.weight,
        })
    }
}
