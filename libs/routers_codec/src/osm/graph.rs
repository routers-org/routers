//! The OSM routing network, held as flat columns: nodes in Z-order, edges in
//! CSR, with each edge's length and road class beside its target.

use alloc::sync::Arc;
use core::fmt::Debug;
use std::sync::{Mutex, OnceLock, PoisonError};

use geo::{Distance, Haversine, Point, Rect};
use log::debug;
use routers_network::edge::Weight;
use routers_network::network::GraphEdge;
use routers_network::{
    DataPlane, Direction, DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Node, Route,
    RowIndex, Scan, envelope_of,
};
use rustc_hash::FxHashMap;
use uom::si::f64::Length;
use uom::si::length::{centimeter, meter};
use web_time::Instant;

#[cfg(not(target_arch = "wasm32"))]
use log::info;
#[cfg(not(target_arch = "wasm32"))]
use rayon::prelude::*;
#[cfg(not(target_arch = "wasm32"))]
use std::io::Write;
#[cfg(not(target_arch = "wasm32"))]
use std::path::{Path, PathBuf};

#[cfg(not(target_arch = "wasm32"))]
use crate::osm::element::ProcessedElement;
use crate::osm::search::{NO_NODE, Search};
use crate::osm::*;

/// A node of the network: an index into its node column.
pub(super) type NodeIx = u32;

/// An edge of the network: an index into its edge columns.
pub(super) type EdgeIx = u32;

type Runtime = <OsmEdgeMetadata as Metadata>::Runtime;

/// Magic header stapled at the start of every `.rt` file.
const SAVE_MAGIC: &[u8; 4] = b"OSMN";

// Prevent files from being used across build revisions
include!(concat!(env!("OUT_DIR"), "/format_hash.rs"));
const SAVE_VERSION: u64 = FORMAT_HASH;

/// The network itself: what is saved, and what a search walks.
#[derive(Debug, Default)]
pub(super) struct Columns {
    nodes: Vec<Node<OsmEntryId>>,

    /// CSR: node `n`'s out-edges are `offsets[n]..offsets[n + 1]`.
    offsets: Vec<u32>,
    pub(super) targets: Vec<NodeIx>,
    /// Per edge: its road-class weight.
    pub(super) class: Vec<Weight>,
    /// Per edge: its Haversine length in whole centimetres.
    pub(super) length_cm: Vec<u32>,
    /// Per edge: the way it belongs to, as an index into `ways`/`meta`.
    way: Vec<u32>,
    /// Per edge: whether it runs against its way's direction.
    backward: Vec<bool>,

    ways: Vec<OsmEntryId>,
    meta: Vec<OsmEdgeMetadata>,
}

impl Columns {
    /// The out-edges of `node`, in the order they were inserted.
    #[inline]
    pub(super) fn out_edges(&self, node: NodeIx) -> core::ops::Range<EdgeIx> {
        self.offsets[node as usize]..self.offsets[node as usize + 1]
    }

    fn point(&self, node: NodeIx) -> Point {
        self.nodes[node as usize].position
    }

    /// Check that the columns describe a graph.
    fn validate(&self) -> Result<(), String> {
        let (nodes, edges) = (self.nodes.len(), self.targets.len());
        let check = |holds: bool, what: &str| holds.then_some(()).ok_or(format!("graph: {what}"));

        check(self.offsets.len() == nodes + 1, "offset column")?;
        check(self.offsets.first() == Some(&0), "offsets start")?;
        check(self.offsets.is_sorted(), "offsets order")?;
        check(
            self.offsets.last().map(|&end| end as usize) == Some(edges),
            "offsets end",
        )?;
        check(
            [
                self.class.len(),
                self.length_cm.len(),
                self.way.len(),
                self.backward.len(),
            ]
            .iter()
            .all(|&len| len == edges),
            "edge columns",
        )?;
        check(
            self.targets.iter().all(|&target| (target as usize) < nodes),
            "edge targets",
        )?;
        check(self.meta.len() == self.ways.len(), "way columns")?;
        check(
            self.way
                .iter()
                .all(|&way| way == u32::MAX || (way as usize) < self.ways.len()),
            "edge ways",
        )
    }
}

pub struct OsmNetwork {
    columns: Columns,

    // Everything below is derived from the columns.
    node_of: FxHashMap<OsmEntryId, NodeIx>,
    way_of: FxHashMap<OsmEntryId, u32>,
    /// Per edge: the node it leaves.
    sources: Vec<NodeIx>,

    node_index: RowIndex<NodeIx>,
    edge_index: RowIndex<EdgeIx>,
    /// In-edges per node, built the first time they are asked for.
    incoming: OnceLock<Vec<Vec<EdgeIx>>>,

    /// Idle search workspaces, one per thread that has searched.
    searches: Mutex<Vec<Search>>,
    /// Which edges each runtime seen so far may traverse.
    passable: Mutex<Vec<(Runtime, Arc<[bool]>)>>,
}

impl Default for OsmNetwork {
    fn default() -> Self {
        Self::new(Columns {
            offsets: vec![0],
            ..Columns::default()
        })
    }
}

impl Debug for OsmNetwork {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("open street maps : network")
    }
}

impl OsmNetwork {
    /// Derive the lookups and spatial indices over `columns`.
    fn new(columns: Columns) -> Self {
        let node_of = columns
            .nodes
            .iter()
            .enumerate()
            .map(|(node, entry)| (entry.id, node as NodeIx))
            .collect();
        let way_of = columns
            .ways
            .iter()
            .enumerate()
            .map(|(way, &id)| (id, way as u32))
            .collect();

        let mut sources = vec![0 as NodeIx; columns.targets.len()];
        for node in 0..columns.nodes.len() as NodeIx {
            for edge in columns.out_edges(node) {
                sources[edge as usize] = node;
            }
        }

        let index_nodes = || {
            RowIndex::build((0..columns.nodes.len() as NodeIx).collect(), |&node| {
                let at = columns.point(node);
                (at, at)
            })
        };
        let index_edges = || {
            RowIndex::build((0..columns.targets.len() as EdgeIx).collect(), |&edge| {
                envelope_of(
                    columns.point(sources[edge as usize]),
                    columns.point(columns.targets[edge as usize]),
                )
            })
        };

        #[cfg(feature = "parallel")]
        let (node_index, edge_index) = rayon::join(index_nodes, index_edges);
        #[cfg(not(feature = "parallel"))]
        let (node_index, edge_index) = (index_nodes(), index_edges());

        OsmNetwork {
            columns,
            node_of,
            way_of,
            sources,
            node_index,
            edge_index,
            incoming: OnceLock::new(),
            searches: Mutex::new(Vec::new()),
            passable: Mutex::new(Vec::new()),
        }
    }

    /// Decode an encoded `OsmNetwork` from bytes and rebuild its indices.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, String> {
        const HEADER_LEN: usize = SAVE_MAGIC.len() + 8;

        if bytes.len() < HEADER_LEN || &bytes[..SAVE_MAGIC.len()] != SAVE_MAGIC {
            return Err("Header bytes are missing, try rebuilding the cache.".to_string());
        }

        let version = u64::from_le_bytes(
            bytes[SAVE_MAGIC.len()..HEADER_LEN]
                .try_into()
                .expect("8 bytes"),
        );

        if version != SAVE_VERSION {
            return Err(format!(
                "Header expects {SAVE_VERSION:016x}, got format hash {version:016x}, rebuild the cache."
            ));
        }

        let started = Instant::now();
        let mut payload = &bytes[HEADER_LEN..];
        let payload = &mut payload;

        let columns = Columns {
            nodes: read(payload, |raw: [u8; 24]| {
                let word = |at: usize| raw[at..at + 8].try_into().expect("8 bytes");

                Node::new(
                    Point::new(f64::from_le_bytes(word(8)), f64::from_le_bytes(word(16))),
                    OsmEntryId::node(i64::from_le_bytes(word(0))),
                )
            })?,
            offsets: read(payload, u32::from_le_bytes)?,
            targets: read(payload, u32::from_le_bytes)?,
            class: read(payload, u32::from_le_bytes)?,
            length_cm: read(payload, u32::from_le_bytes)?,
            way: read(payload, u32::from_le_bytes)?,
            backward: read(payload, |[byte]: [u8; 1]| byte != 0)?,
            ways: read(payload, |raw| OsmEntryId::way(i64::from_le_bytes(raw)))?,
            meta: postcard::from_bytes(payload).map_err(|e| e.to_string())?,
        };
        columns.validate()?;

        let deserialise = started.elapsed();
        let network = Self::new(columns);

        debug!(
            "OsmNetwork::from_bytes: {} bytes, deserialised in {:?}, built indices in {:?}",
            bytes.len(),
            deserialise,
            started.elapsed() - deserialise
        );

        Ok(network)
    }

    /// Encode `self` into a `Vec<u8>` with the format header prepended.
    /// Counterpart to [`from_bytes`](Self::from_bytes); filesystem-free.
    pub fn to_bytes(&self) -> Result<Vec<u8>, String> {
        let columns = &self.columns;
        let mut out = Vec::new();

        out.extend_from_slice(SAVE_MAGIC);
        out.extend_from_slice(&SAVE_VERSION.to_le_bytes());

        write(&mut out, &columns.nodes, |node| {
            let mut raw = [0; 24];
            raw[..8].copy_from_slice(&node.id.identifier().to_le_bytes());
            raw[8..16].copy_from_slice(&node.position.x().to_le_bytes());
            raw[16..].copy_from_slice(&node.position.y().to_le_bytes());
            raw
        });
        write(&mut out, &columns.offsets, |v| v.to_le_bytes());
        write(&mut out, &columns.targets, |v| v.to_le_bytes());
        write(&mut out, &columns.class, |v| v.to_le_bytes());
        write(&mut out, &columns.length_cm, |v| v.to_le_bytes());
        write(&mut out, &columns.way, |v| v.to_le_bytes());
        write(&mut out, &columns.backward, |v| [*v as u8]);
        write(&mut out, &columns.ways, |v| v.identifier().to_le_bytes());

        let meta = postcard::to_allocvec(&columns.meta)
            .map_err(|e| format!("failed to serialise value: {e}"))?;
        out.extend_from_slice(&meta);

        Ok(out)
    }

    /// Build an `OsmNetwork` either from a cached `.rt` file (fast path)
    /// or, if the cache is missing or stale, from the source PBF (slow
    /// path, writes the cache for next time).
    ///
    /// Filesystem-bound; not available on WASM targets — use
    /// [`from_bytes`](Self::from_bytes) with HTTP-fetched cache bytes
    /// instead.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_pbf_and_save(pbf_path: &PathBuf, saved_path: &PathBuf) -> Result<Self, String> {
        if saved_path.exists() {
            match OsmNetwork::from_saved(saved_path) {
                Ok(g) => return Ok(g),
                Err(e) => {
                    log::warn!(
                        "OsmNetwork cache at `{}` is unusable ({e}); rebuilding from PBF",
                        saved_path.display()
                    );
                }
            }
        }
        let graph = OsmNetwork::from_pbf(pbf_path).map_err(|e| e.to_string())?;
        graph.save_to_file(saved_path)?;
        Ok(graph)
    }

    /// Read a saved `.rt` from disk into an `OsmNetwork`. Thin wrapper
    /// around [`from_bytes`](Self::from_bytes); not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_saved(filename: &PathBuf) -> Result<Self, String> {
        let bytes = std::fs::read(filename).map_err(|v| v.to_string())?;
        Self::from_bytes(&bytes).map_err(|e| format!("cache file `{}`: {e}", filename.display()))
    }

    /// Persist this network to disk. Thin wrapper around
    /// [`to_bytes`](Self::to_bytes); not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn save_to_file(&self, path: &Path) -> Result<(), String> {
        let bytes = self.to_bytes()?;
        let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
        file.write_all(&bytes).map_err(|e| e.to_string())?;
        debug!(
            "OsmNetwork::save_to_file wrote {} bytes (incl. 12-byte header, format {:016x}) to {}",
            bytes.len(),
            SAVE_VERSION,
            path.display()
        );
        Ok(())
    }

    /// Construct an `OsmNetwork` from a `.osm.pbf` file. Uses memory-mapped
    /// IO, multithreaded parsing and rayon; not available on WASM.
    #[cfg(not(target_arch = "wasm32"))]
    pub fn from_pbf(filename: &PathBuf) -> Result<Self, Box<dyn core::error::Error>> {
        let started = Instant::now();

        let reader =
            ProcessedElementIterator::new(filename.clone()).map_err(|err| format!("{err:?}"))?;

        info!("Ingesting...");

        type Parts = (
            Vec<Node<OsmEntryId>>,
            Vec<RawEdge>,
            Vec<(OsmEntryId, OsmEdgeMetadata)>,
        );

        let (nodes, edges, ways): Parts = reader.par_red(
            |mut parts: Parts, element: ProcessedElement| {
                match element {
                    ProcessedElement::Way(way) => {
                        let metadata = OsmEdgeMetadata::pick(way.tags());

                        let Some(class) = metadata.road_class.map(|class| class.weighting()) else {
                            return parts;
                        };

                        let bidirectional = !way.tags().unidirectional();
                        parts.2.push((way.id(), metadata));

                        for pair in way.refs().windows(2) {
                            let [a, b] = pair else { continue };

                            parts.1.push(RawEdge {
                                source: a.id,
                                target: b.id,
                                class,
                                way: way.id(),
                                direction: Direction::Outgoing,
                            });
                            if bidirectional {
                                parts.1.push(RawEdge {
                                    source: b.id,
                                    target: a.id,
                                    class,
                                    way: way.id(),
                                    direction: Direction::Incoming,
                                });
                            }
                        }
                    }
                    ProcessedElement::Node(node) => parts.0.push(node),
                    _ => {}
                }

                parts
            },
            |mut a, b| {
                a.0.extend(b.0);
                a.1.extend(b.1);
                a.2.extend(b.2);
                a
            },
            || (Vec::new(), Vec::new(), Vec::new()),
        );

        debug!("Parsing took: {:?}", started.elapsed());

        let total = nodes.len();
        let network = Self::new(assemble(nodes, edges, ways));

        info!(
            "Finished. Ingested {:?} nodes from {:?} nodes total in {}ms",
            network.num_nodes(),
            total,
            started.elapsed().as_millis()
        );

        Ok(network)
    }

    pub fn num_nodes(&self) -> usize {
        self.columns.nodes.len()
    }

    pub fn num_edges(&self) -> usize {
        self.columns.targets.len()
    }

    /// Every node of the network.
    pub fn nodes(&self) -> impl Iterator<Item = &Node<OsmEntryId>> {
        self.columns.nodes.iter()
    }

    /// Every directed edge of the network, grouped by the node it leaves.
    pub fn edges(&self) -> impl Iterator<Item = GraphEdge<OsmEntryId>> + '_ {
        (0..self.num_edges() as EdgeIx).map(|edge| self.graph_edge(edge))
    }

    /// Run `search` with a workspace from the pool.
    fn searching<T>(&self, search: impl FnOnce(&mut Search) -> T) -> T {
        let idle = self
            .searches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop();
        let mut workspace = idle.unwrap_or_else(|| Search::new(self.num_nodes()));

        let found = search(&mut workspace);

        self.searches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(workspace);
        found
    }

    /// The edges `runtime` may traverse, computed once per runtime.
    fn passable(&self, runtime: &Runtime) -> Arc<[bool]> {
        let known = |masks: &[(Runtime, Arc<[bool]>)]| {
            masks
                .iter()
                .find(|(seen, _)| seen == runtime)
                .map(|(_, mask)| Arc::clone(mask))
        };

        if let Some(mask) = known(&self.passable.lock().unwrap_or_else(PoisonError::into_inner)) {
            return mask;
        }

        // Built with the lock released: a caller may be on a thread pool
        // that hands it other work, which must not find this lock held.
        let columns = &self.columns;
        let allowed = |direction| {
            columns
                .meta
                .iter()
                .map(|meta| meta.accessible(runtime, direction))
                .collect::<Vec<_>>()
        };
        let (forward, backward) = (allowed(Direction::Outgoing), allowed(Direction::Incoming));

        let mask: Arc<[bool]> = columns
            .way
            .iter()
            .zip(&columns.backward)
            .map(|(&way, &against)| {
                let allowed = if against { &backward } else { &forward };
                allowed.get(way as usize).copied().unwrap_or(false)
            })
            .collect();

        let mut masks = self.passable.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(raced) = known(&masks) {
            return raced;
        }
        masks.push((runtime.clone(), Arc::clone(&mask)));
        mask
    }

    fn way_id(&self, edge: EdgeIx) -> DirectionAwareEdgeId<OsmEntryId> {
        let columns = &self.columns;
        let way = columns.way[edge as usize] as usize;
        let id = columns.ways.get(way).copied().unwrap_or_default();

        DirectionAwareEdgeId::new(id).with_direction(if columns.backward[edge as usize] {
            Direction::Incoming
        } else {
            Direction::Outgoing
        })
    }

    fn graph_edge(&self, edge: EdgeIx) -> GraphEdge<OsmEntryId> {
        let columns = &self.columns;

        (
            columns.nodes[self.sources[edge as usize] as usize].id,
            columns.nodes[columns.targets[edge as usize] as usize].id,
            (columns.class[edge as usize], self.way_id(edge)),
        )
    }
}

/// One directed edge as ingested, before it is laid out.
#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, Debug)]
struct RawEdge {
    source: OsmEntryId,
    target: OsmEntryId,
    class: Weight,
    way: OsmEntryId,
    direction: Direction,
}

/// Lay ingested parts out as columns.
///
/// `edges` are in insertion order: a repeated `(source, target)` keeps its
/// first position and takes its last data.
#[cfg(not(target_arch = "wasm32"))]
fn assemble(
    mut nodes: Vec<Node<OsmEntryId>>,
    edges: Vec<RawEdge>,
    mut ways: Vec<(OsmEntryId, OsmEdgeMetadata)>,
) -> Columns {
    let mut endpoints = edges
        .par_iter()
        .flat_map_iter(|edge| [edge.source, edge.target])
        .collect::<Vec<_>>();
    endpoints.par_sort_unstable();
    endpoints.dedup();

    nodes.par_sort_unstable_by_key(|node| node.id);
    nodes.dedup_by_key(|node| node.id);
    let position = |id: OsmEntryId| {
        nodes
            .binary_search_by_key(&id, |node| node.id)
            .map_or(Point::new(f64::NAN, f64::NAN), |found| {
                nodes[found].position
            })
    };

    let mut placed = endpoints
        .par_iter()
        .map(|&id| {
            let at = position(id);
            (morton(at), Node::new(at, id))
        })
        .collect::<Vec<_>>();
    placed.par_sort_unstable_by_key(|&(code, node)| (code, node.id));
    let nodes = placed.into_iter().map(|(_, node)| node).collect::<Vec<_>>();

    // `endpoints` is sorted by id: its inverse permutation onto the Z-order
    // numbering turns an id into a node by binary search.
    let mut by_id = (0..nodes.len() as NodeIx).collect::<Vec<_>>();
    by_id.par_sort_unstable_by_key(|&node| nodes[node as usize].id);
    let node_of = |id: OsmEntryId| {
        let found = endpoints
            .binary_search(&id)
            .expect("an edge endpoint is a node");
        by_id[found]
    };

    ways.par_sort_unstable_by_key(|&(id, _)| id);
    ways.dedup_by_key(|&mut (id, _)| id);
    let way_of = |id: OsmEntryId| {
        ways.binary_search_by_key(&id, |&(id, _)| id)
            .map_or(u32::MAX, |found| found as u32)
    };

    let resolved = edges
        .par_iter()
        .map(|edge| (node_of(edge.source), node_of(edge.target), *edge))
        .collect::<Vec<_>>();
    drop(edges);

    // Counting sort by source: stable, so each node's edges stay in
    // insertion order.
    let mut offsets = vec![0u32; nodes.len() + 1];
    for &(source, _, _) in &resolved {
        offsets[source as usize + 1] += 1;
    }
    for node in 0..nodes.len() {
        offsets[node + 1] += offsets[node];
    }

    let mut cursor = offsets.clone();
    let mut slots = vec![u32::MAX; resolved.len()];
    for (index, &(source, _, _)) in resolved.iter().enumerate() {
        let slot = &mut cursor[source as usize];
        slots[*slot as usize] = index as u32;
        *slot += 1;
    }
    drop(cursor);

    // Emit each node's edges, folding repeats of a target into the first.
    let mut columns = Columns {
        targets: Vec::with_capacity(resolved.len()),
        class: Vec::with_capacity(resolved.len()),
        way: Vec::with_capacity(resolved.len()),
        backward: Vec::with_capacity(resolved.len()),
        ..Columns::default()
    };
    columns.offsets.push(0);

    for node in 0..nodes.len() {
        let start = columns.targets.len();

        for &slot in &slots[offsets[node] as usize..offsets[node + 1] as usize] {
            let (_, target, edge) = resolved[slot as usize];
            let (way, backward) = (way_of(edge.way), edge.direction == Direction::Incoming);

            let repeat = columns.targets[start..]
                .iter()
                .position(|&seen| seen == target);
            match repeat {
                Some(first) => {
                    columns.class[start + first] = edge.class;
                    columns.way[start + first] = way;
                    columns.backward[start + first] = backward;
                }
                None => {
                    columns.targets.push(target);
                    columns.class.push(edge.class);
                    columns.way.push(way);
                    columns.backward.push(backward);
                }
            }
        }

        columns.offsets.push(columns.targets.len() as u32);
    }

    columns.nodes = nodes;
    (columns.ways, columns.meta) = ways.into_iter().unzip();

    columns.length_cm = (0..columns.nodes.len() as NodeIx)
        .into_par_iter()
        .flat_map_iter(|node| {
            let from = columns.point(node);
            columns.out_edges(node).map(move |edge| (from, edge))
        })
        .map(|(from, edge)| {
            let to = columns.point(columns.targets[edge as usize]);
            Length::new::<meter>(Haversine.distance(from, to)).get::<centimeter>() as u32
        })
        .collect();

    columns
}

/// Interleave the bits of a quantised position (Z-order).
#[cfg(not(target_arch = "wasm32"))]
fn morton(at: Point) -> u64 {
    fn spread(value: u32) -> u64 {
        let mut v = u64::from(value);
        v = (v | (v << 16)) & 0x0000_FFFF_0000_FFFF;
        v = (v | (v << 8)) & 0x00FF_00FF_00FF_00FF;
        v = (v | (v << 4)) & 0x0F0F_0F0F_0F0F_0F0F;
        v = (v | (v << 2)) & 0x3333_3333_3333_3333;
        v = (v | (v << 1)) & 0x5555_5555_5555_5555;
        v
    }

    if at.x().is_nan() || at.y().is_nan() {
        return u64::MAX;
    }

    let x = ((at.x() + 180.0) / 360.0 * f64::from(u32::MAX)) as u32;
    let y = ((at.y() + 90.0) / 180.0 * f64::from(u32::MAX)) as u32;
    spread(x) | (spread(y) << 1)
}

/// Append a column: its length, then each value's little-endian bytes.
fn write<T, const N: usize>(out: &mut Vec<u8>, values: &[T], bytes: impl Fn(&T) -> [u8; N]) {
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    out.extend(values.iter().flat_map(bytes));
}

/// Read a column [written](write) at the head of `bytes`.
fn read<T, const N: usize>(
    bytes: &mut &[u8],
    value: impl Fn([u8; N]) -> T,
) -> Result<Vec<T>, String> {
    let mut take = |len: usize| {
        let (head, tail) = bytes.split_at_checked(len).ok_or("truncated graph")?;
        *bytes = tail;
        Ok::<_, String>(head)
    };

    let len = u64::from_le_bytes(take(8)?.try_into().expect("8 bytes")) as usize;
    let size = len.checked_mul(N).ok_or("graph column too long")?;

    Ok(take(size)?
        .chunks_exact(N)
        .map(|chunk| value(chunk.try_into().expect("chunk of N")))
        .collect())
}

impl DataPlane for OsmNetwork {
    type Entry = OsmEntryId;
    type Runtime = Runtime;
    type Meta = OsmEdgeMetadata;

    fn metadata(&self, id: &OsmEntryId) -> Option<&OsmEdgeMetadata> {
        self.columns.meta.get(*self.way_of.get(id)? as usize)
    }

    fn point(&self, id: &OsmEntryId) -> Option<Point> {
        self.node_of.get(id).map(|&node| self.columns.point(node))
    }

    fn edges_into<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        let incoming = self.incoming.get_or_init(|| {
            let mut incoming = vec![Vec::new(); self.num_nodes()];
            for (edge, &target) in self.columns.targets.iter().enumerate() {
                incoming[target as usize].push(edge as EdgeIx);
            }
            incoming
        });
        let edges = self.node_of.get(&id).map(|&node| &incoming[node as usize]);

        Box::new(
            edges
                .into_iter()
                .flatten()
                .map(|&edge| self.graph_edge(edge)),
        )
    }

    fn edges_outof<'a>(
        &'a self,
        id: OsmEntryId,
    ) -> Box<dyn Iterator<Item = GraphEdge<OsmEntryId>> + 'a> {
        let edges = self
            .node_of
            .get(&id)
            .map(|&node| self.columns.out_edges(node));

        Box::new(
            edges
                .into_iter()
                .flatten()
                .map(|edge| self.graph_edge(edge)),
        )
    }

    fn fatten(&self, edge: &Edge<OsmEntryId>) -> Option<Edge<Node<OsmEntryId>>> {
        Some(Edge {
            source: *self.node(&edge.source)?,
            target: *self.node(&edge.target)?,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), edge.id.index())),
            weight: edge.weight,
        })
    }

    fn reach(
        &self,
        root: &OsmEntryId,
        reach: f64,
        runtime: &Runtime,
    ) -> Option<Vec<(OsmEntryId, OsmEntryId)>> {
        let root = *self.node_of.get(root)?;
        let passable = self.passable(runtime);
        let nodes = &self.columns.nodes;

        Some(self.searching(|search| {
            search
                .reach(&self.columns, &passable, root, Length::new::<meter>(reach))
                .iter()
                .map(|&(node, parent)| {
                    let parent = match parent {
                        NO_NODE => OsmEntryId::default(),
                        parent => nodes[parent as usize].id,
                    };
                    (nodes[node as usize].id, parent)
                })
                .collect()
        }))
    }
}

impl Discovery for OsmNetwork {
    fn edges_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = Edge<Node<OsmEntryId>>> + Send + 'a> {
        let columns = &self.columns;

        Box::new(self.edge_index.search(bounds).map(move |&edge| {
            let way = self.way_id(edge);

            Edge {
                source: columns.nodes[self.sources[edge as usize] as usize],
                target: columns.nodes[columns.targets[edge as usize] as usize],
                id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), way.index()))
                    .with_direction(way.direction()),
                weight: columns.class[edge as usize],
            }
        }))
    }

    fn nodes_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = &'a Node<OsmEntryId>> + Send + 'a> {
        Box::new(
            self.node_index
                .search(bounds)
                .map(|&node| &self.columns.nodes[node as usize]),
        )
    }

    fn node(&self, id: &OsmEntryId) -> Option<&Node<OsmEntryId>> {
        self.node_of
            .get(id)
            .map(|&node| &self.columns.nodes[node as usize])
    }

    fn edge(&self, source: &OsmEntryId, target: &OsmEntryId) -> Option<Edge<OsmEntryId>> {
        let (from, to) = (*self.node_of.get(source)?, *self.node_of.get(target)?);
        let edge = self
            .columns
            .out_edges(from)
            .find(|&edge| self.columns.targets[edge as usize] == to)?;

        Some(Edge {
            source: *source,
            target: *target,
            weight: self.columns.class[edge as usize],
            id: self.way_id(edge),
        })
    }
}

impl Scan for OsmNetwork {
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<OsmEntryId>> {
        self.node_index
            .nearest(point)
            .map(|&node| &self.columns.nodes[node as usize])
    }
}

impl Route for OsmNetwork {
    fn route_nodes(
        &self,
        start_node: OsmEntryId,
        finish_node: OsmEntryId,
    ) -> Option<(Weight, Vec<Node<OsmEntryId>>)> {
        let (from, to) = (
            *self.node_of.get(&start_node)?,
            *self.node_of.get(&finish_node)?,
        );
        let (cost, path) = self.searching(|search| search.route(&self.columns, from, to))?;

        Some((
            cost,
            path.into_iter()
                .map(|node| self.columns.nodes[node as usize])
                .collect(),
        ))
    }
}
