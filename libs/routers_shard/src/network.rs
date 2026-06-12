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
// `web_time::Instant` is a drop-in for `std::time::Instant` that doesn't
// panic on `wasm32-unknown-unknown` (`std::time` has no clock source there).
use web_time::Instant;

use routers_network::{
    DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Node, Route, Scan, edge::Weight,
    network::GraphEdge,
};

use crate::selection::Selection;
use crate::strategy::{ShardId, ShardingStrategy};

pub type GraphStructure<E> =
    DiGraphMap<E, (Weight, DirectionAwareEdgeId<E>), BuildHasherDefault<FxHasher>>;

/// Slim spatial-index entry for edges. Holds just the endpoint ids and
/// the cached AABB needed for rstar's envelope queries. Full
/// `Edge<Node<E>>` values are materialised on demand from `hash` +
/// `graph` lookups — ~3× smaller than storing fat edges in the RTree,
/// which dominated runtime RAM at precision-4 shard sizes.
#[derive(Clone, Copy, Debug)]
pub struct EdgeRef<E: Entry> {
    pub source: E,
    pub target: E,
    bbox_min: Point,
    bbox_max: Point,
}

impl<E: Entry> EdgeRef<E> {
    pub fn new(source: E, target: E, src_pos: Point, tgt_pos: Point) -> Self {
        let (sx, sy) = src_pos.x_y();
        let (tx, ty) = tgt_pos.x_y();
        Self {
            source,
            target,
            bbox_min: Point::new(sx.min(tx), sy.min(ty)),
            bbox_max: Point::new(sx.max(tx), sy.max(ty)),
        }
    }
}

impl<E: Entry> rstar::RTreeObject for EdgeRef<E> {
    type Envelope = AABB<Point>;
    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(self.bbox_min, self.bbox_max)
    }
}

/// A data source from which a [`ShardedNetwork`] can be built.
///
/// Implement this trait on any type that provides an iterable collection of
/// nodes (id + position) and directed edges (from, to, weight, metadata).
/// [`ShardedNetwork::from_source`] then filters and assembles these into the
/// sharded graph structure.
pub trait ShardSource<E: Entry, M: Metadata> {
    fn nodes<'a>(&'a self) -> Box<dyn Iterator<Item = (E, Point)> + 'a>;
    fn edges<'a>(&'a self) -> Box<dyn Iterator<Item = (E, E, Weight, M)> + 'a>;
}

/// Magic header + format fingerprint prepended to every shard cache file.
///
/// `CACHE_VERSION` is computed at build time (see `build.rs`), to prevent
/// files from being reused across incompatible code versions.
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

    /// Spatial index over nodes.
    #[serde(skip)]
    pub index: RTree<Node<E>>,

    /// Slim spatial index over edges — stores only ids + cached AABB,
    /// not the fat `Edge<Node<E>>` value. We do still need an edge
    /// RTree (an earlier attempt to derive edges from `index` +
    /// adjacency missed any edge passing near a GPS point whose
    /// endpoints fell outside the search box, and torched success rate).
    #[serde(skip)]
    pub index_edge: RTree<EdgeRef<E>>,

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
    pub fn num_nodes(&self) -> usize {
        self.graph.node_count()
    }

    pub fn num_edges(&self) -> usize {
        self.graph.edge_count()
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
        let mut graph: GraphStructure<E> = GraphStructure::new();
        let mut hash: FxHashMap<E, Node<E>> = FxHashMap::default();
        let mut meta: FxHashMap<E, M> = FxHashMap::default();

        // Collect all node positions up-front so we can look up boundary
        // nodes (those outside the selection that are targets of boundary
        // edges) without a second source scan.
        let all_nodes: FxHashMap<E, Point> = source.nodes().collect();

        // Admit a node if either (a) its shard is in the loaded set, or
        // (b) its position falls within the optional padded buffer. The
        // padding path lets `OwnedAndPadded` pull in cross-boundary
        // road-network data without materialising whole neighbour shards.
        for (&id, &pos) in &all_nodes {
            let admit = selection.padding_contains(pos) || {
                let shard = strategy.locate(pos);
                selection.contains(&shard)
            };
            if admit {
                hash.insert(id, Node::new(pos, id));
                graph.add_node(id);
            }
        }

        // Include every edge whose source is in the selection. If the target
        // is outside the selection, add it as a context node so the boundary
        // edge is traversable when shards are merged at runtime.
        for (from, to, weight, m) in source.edges() {
            if !hash.contains_key(&from) {
                continue;
            }
            if !hash.contains_key(&to) {
                if let Some(&pos) = all_nodes.get(&to) {
                    hash.insert(to, Node::new(pos, to));
                    graph.add_node(to);
                } else {
                    continue;
                }
            }
            let edge_id = DirectionAwareEdgeId::new(from);
            graph.add_edge(from, to, (weight, edge_id));
            meta.entry(from).or_insert(m);
        }

        let mut net = Self {
            graph,
            hash,
            meta,
            index: RTree::new(),
            index_edge: RTree::new(),
            owned: selection.owned,
            loaded: selection.loaded.clone(),
        };
        net.rebuild_indices();
        Ok(net)
    }

    /// Rebuild the spatial indices from `hash` + `graph`.
    ///
    /// Both the node RTree and the slim edge RTree are intentionally not
    /// serialised — bulk-loading is O(N log N) at hundreds of MB/s,
    /// faster than letting `postcard` decode tree structures that take
    /// proportionally more bytes on disk.
    pub fn rebuild_indices(&mut self) {
        let nodes: Vec<Node<E>> = self.hash.values().copied().collect();
        let edges: Vec<EdgeRef<E>> = self
            .graph
            .all_edges()
            .filter_map(|(s, t, _)| {
                let src_pos = self.hash.get(&s)?.position;
                let tgt_pos = self.hash.get(&t)?.position;
                Some(EdgeRef::new(s, t, src_pos, tgt_pos))
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
    ) -> Box<dyn Iterator<Item = Edge<Node<E>>> + Send + 'a>
    where
        E: 'a,
    {
        Box::new(
            self.index_edge
                .locate_in_envelope_intersecting(&aabb)
                .filter_map(move |edge_ref| {
                    let source = *self.hash.get(&edge_ref.source)?;
                    let target = *self.hash.get(&edge_ref.target)?;
                    let &(weight, id) =
                        self.graph.edge_weight(edge_ref.source, edge_ref.target)?;
                    Some(Edge {
                        source,
                        target,
                        id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), id.index()))
                            .with_direction(id.direction()),
                        weight,
                    })
                }),
        )
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
