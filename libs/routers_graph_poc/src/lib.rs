//! Proof of concept: a compact, array-backed routing graph.
//!
//! [`OsmNetwork`] keeps its topology in a hash-keyed `DiGraphMap`, its node
//! positions in a second hash map and its way metadata in a third. Every step
//! of a search hashes an OSM id several times, and the per-node successor
//! lists the matcher needs are computed lazily and held in yet another cache.
//!
//! [`CompactGraph`] holds the same network as a handful of flat arrays:
//!
//! - nodes are dense `u32` indices, numbered along a space-filling curve so
//!   that neighbours on the road are neighbours in memory;
//! - positions are two `f64` columns (structure-of-arrays);
//! - adjacency is CSR — one offset per node into contiguous edge columns —
//!   with each edge's length and road class stored beside its target, which
//!   is everything a search reads.
//!
//! OSM ids are translated at the boundary only: once on the way in, once on
//! the way out.
//!
//! [`Search`] is the matcher's bounded Dijkstra over that layout. It settles
//! the same nodes, in the same order, with the same parents as
//! `routers_transition`'s — the cost model and tie-breaking are reproduced
//! exactly — but against generation-stamped arrays instead of hash maps, and
//! without allocating once warm.

extern crate alloc;

mod network;

pub use network::CompactNetwork;

use alloc::collections::BinaryHeap;
use core::cmp::Ordering;
use std::path::Path;

use geo::{Distance, Haversine, Point};
use rayon::prelude::*;
use routers_codec::osm::element::ProcessedElement;
use routers_codec::osm::{
    OsmEdgeMetadata, OsmEntryId, OsmNetwork, Parallel, ProcessedElementIterator,
};
use routers_network::{Direction, Entry, Metadata, RowIndex, envelope_of};
use rustc_hash::FxHashMap;
use uom::si::f64::Length;
use uom::si::length::{centimeter, meter};

/// A node of a [`CompactGraph`]: an index into its node columns.
pub type NodeIx = u32;

/// An edge of a [`CompactGraph`]: an index into its edge columns.
pub type EdgeIx = u32;

/// "No such node", where a `NodeIx` is optional.
pub const NO_NODE: NodeIx = NodeIx::MAX;

/// Leads every serialised graph, naming the format and its version.
const MAGIC: &[u8; 8] = b"RTRCSR01";

/// One directed edge as ingested, before it is laid out.
#[derive(Clone, Copy, Debug)]
struct RawEdge {
    source: i64,
    target: i64,
    class: u32,
    way: i64,
    direction: Direction,
}

/// A routing network as flat arrays. See the [crate docs](crate).
#[derive(Debug, Default)]
pub struct CompactGraph {
    /// OSM id of each node.
    ids: Vec<i64>,
    /// Longitude and latitude of each node, in degrees.
    lng: Vec<f64>,
    lat: Vec<f64>,

    /// CSR: node `n`'s out-edges are `offsets[n]..offsets[n + 1]`.
    offsets: Vec<u32>,
    /// Per edge: the node it leads to.
    targets: Vec<NodeIx>,
    /// Per edge: its road-class weight.
    class: Vec<u32>,
    /// Per edge: its Haversine length in whole centimetres.
    length_cm: Vec<u32>,
    /// Per edge: the way it belongs to, as an index into `ways`/`meta`.
    way: Vec<u32>,
    /// Per edge: whether it runs against its way's direction.
    backward: Vec<bool>,

    /// OSM id and metadata of each way.
    ways: Vec<i64>,
    meta: Vec<OsmEdgeMetadata>,
}

impl CompactGraph {
    /// Ingest a `.osm.pbf` directly, without building an [`OsmNetwork`].
    pub fn from_pbf(path: &Path) -> Result<Self, String> {
        let reader =
            ProcessedElementIterator::new(path.to_path_buf()).map_err(|e| format!("{e:?}"))?;

        type Parts = (Vec<(i64, Point)>, Vec<RawEdge>, Vec<(i64, OsmEdgeMetadata)>);

        let (nodes, edges, ways): Parts = reader.par_red(
            |mut parts: Parts, element: ProcessedElement| {
                match element {
                    ProcessedElement::Way(way) => {
                        let metadata = OsmEdgeMetadata::pick(way.tags());
                        let Some(class) = metadata.road_class.map(|class| class.weighting()) else {
                            return parts;
                        };

                        let bidirectional = !way.tags().unidirectional();
                        let id = way.id().identifier();
                        parts.2.push((id, metadata));

                        for pair in way.refs().windows(2) {
                            let [a, b] = pair else { continue };
                            let (a, b) = (a.id.identifier(), b.id.identifier());

                            parts.1.push(RawEdge {
                                source: a,
                                target: b,
                                class,
                                way: id,
                                direction: Direction::Outgoing,
                            });
                            if bidirectional {
                                parts.1.push(RawEdge {
                                    source: b,
                                    target: a,
                                    class,
                                    way: id,
                                    direction: Direction::Incoming,
                                });
                            }
                        }
                    }
                    ProcessedElement::Node(node) => {
                        parts.0.push((node.id.identifier(), node.position));
                    }
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

        Ok(Self::assemble(nodes, edges, ways))
    }

    /// Lay an existing [`OsmNetwork`] out compactly.
    pub fn from_network(network: &OsmNetwork) -> Self {
        let nodes = network
            .hash
            .values()
            .map(|node| (node.id.identifier(), node.position))
            .collect();

        // In the graph's own order: a node's out-edges keep the order the
        // matcher's search would meet them in.
        let edges = network
            .graph
            .nodes()
            .flat_map(|source| {
                network
                    .graph
                    .edges_directed(source, petgraph::Direction::Outgoing)
                    .map(|(source, target, &(class, id))| RawEdge {
                        source: source.identifier(),
                        target: target.identifier(),
                        class,
                        way: id.index().identifier(),
                        direction: id.direction(),
                    })
            })
            .collect();

        let ways = network
            .meta
            .iter()
            .map(|(id, meta)| (id.identifier(), meta.clone()))
            .collect();

        Self::assemble(nodes, edges, ways)
    }

    /// Build the columns from ingested parts.
    ///
    /// `edges` are in insertion order. As in a `DiGraphMap`, a repeated
    /// `(source, target)` keeps its first position and takes its last data.
    fn assemble(
        mut nodes: Vec<(i64, Point)>,
        edges: Vec<RawEdge>,
        mut ways: Vec<(i64, OsmEdgeMetadata)>,
    ) -> Self {
        // The nodes of the graph are the endpoints of its edges; the PBF also
        // carries nodes no road touches.
        let mut endpoints = edges
            .par_iter()
            .flat_map_iter(|edge| [edge.source, edge.target])
            .collect::<Vec<_>>();
        endpoints.par_sort_unstable();
        endpoints.dedup();

        nodes.par_sort_unstable_by_key(|&(id, _)| id);
        nodes.dedup_by_key(|&mut (id, _)| id);
        let position = |id: i64| {
            nodes
                .binary_search_by_key(&id, |&(id, _)| id)
                .map_or(Point::new(f64::NAN, f64::NAN), |found| nodes[found].1)
        };

        // Number the nodes along a Z-order curve: a search then walks memory
        // roughly the way it walks the road.
        let mut placed = endpoints
            .par_iter()
            .map(|&id| {
                let at = position(id);
                (morton(at), id, at)
            })
            .collect::<Vec<_>>();
        placed.par_sort_unstable_by_key(|&(code, id, _)| (code, id));

        let ids = placed.iter().map(|&(_, id, _)| id).collect::<Vec<_>>();
        let lng = placed.iter().map(|&(_, _, at)| at.x()).collect::<Vec<_>>();
        let lat = placed.iter().map(|&(_, _, at)| at.y()).collect::<Vec<_>>();
        drop(placed);

        // `endpoints` is sorted by id: its inverse permutation onto the
        // Z-order numbering turns an id into a node by binary search.
        let mut by_id = (0..ids.len() as u32).collect::<Vec<_>>();
        by_id.par_sort_unstable_by_key(|&node| ids[node as usize]);
        let node_of = |id: i64| {
            let found = endpoints
                .binary_search(&id)
                .expect("an edge endpoint is a node");
            by_id[found]
        };

        ways.par_sort_unstable_by_key(|&(id, _)| id);
        ways.dedup_by_key(|&mut (id, _)| id);
        let way_of = |id: i64| {
            ways.binary_search_by_key(&id, |&(id, _)| id)
                .map_or(u32::MAX, |found| found as u32)
        };

        // Resolve ids once, in parallel; everything after is index arithmetic.
        let resolved = edges
            .par_iter()
            .map(|edge| (node_of(edge.source), node_of(edge.target), *edge))
            .collect::<Vec<_>>();
        drop(edges);

        // Counting sort by source: stable, so each node's edges stay in
        // insertion order.
        let mut offsets = vec![0u32; ids.len() + 1];
        for &(source, _, _) in &resolved {
            offsets[source as usize + 1] += 1;
        }
        for node in 0..ids.len() {
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
        let mut graph = CompactGraph {
            targets: Vec::with_capacity(resolved.len()),
            class: Vec::with_capacity(resolved.len()),
            way: Vec::with_capacity(resolved.len()),
            backward: Vec::with_capacity(resolved.len()),
            ..CompactGraph::default()
        };
        let mut compacted = Vec::with_capacity(ids.len() + 1);
        compacted.push(0u32);

        for node in 0..ids.len() {
            let start = graph.targets.len();

            for &slot in &slots[offsets[node] as usize..offsets[node + 1] as usize] {
                let (_, target, edge) = resolved[slot as usize];

                let repeat = graph.targets[start..]
                    .iter()
                    .position(|&seen| seen == target);
                match repeat {
                    Some(first) => {
                        graph.class[start + first] = edge.class;
                        graph.way[start + first] = way_of(edge.way);
                        graph.backward[start + first] = edge.direction == Direction::Incoming;
                    }
                    None => {
                        graph.targets.push(target);
                        graph.class.push(edge.class);
                        graph.way.push(way_of(edge.way));
                        graph.backward.push(edge.direction == Direction::Incoming);
                    }
                }
            }

            compacted.push(graph.targets.len() as u32);
        }

        graph.offsets = compacted;
        graph.ids = ids;
        graph.lng = lng;
        graph.lat = lat;
        (graph.ways, graph.meta) = ways.into_iter().unzip();

        // What the matcher's successor cache computes lazily, per node, on
        // first visit: here once, for every edge, across all cores.
        graph.length_cm = (0..graph.ids.len() as u32)
            .into_par_iter()
            .flat_map_iter(|node| {
                let from = graph.point(node);
                graph.out_edges(node).map(move |edge| (from, edge))
            })
            .map(|(from, edge)| {
                let to = graph.point(graph.targets[edge as usize]);
                Length::new::<meter>(Haversine.distance(from, to)).get::<centimeter>() as u32
            })
            .collect();

        graph
    }

    /// Whether `other` holds the same network, column for column (metadata
    /// aside, which has no equality).
    pub fn same_layout(&self, other: &Self) -> bool {
        self.ids == other.ids
            && self.lng == other.lng
            && self.lat == other.lat
            && self.offsets == other.offsets
            && self.targets == other.targets
            && self.class == other.class
            && self.length_cm == other.length_cm
            && self.way == other.way
            && self.backward == other.backward
            && self.ways == other.ways
            && self.meta.len() == other.meta.len()
    }

    pub fn nodes(&self) -> usize {
        self.ids.len()
    }

    pub fn edges(&self) -> usize {
        self.targets.len()
    }

    /// The node with OSM id `id`, by binary search over a sorted column.
    ///
    /// A production layout would keep that column; the PoC builds it on
    /// demand through [`Lookup`].
    pub fn lookup(&self) -> Lookup {
        Lookup(
            self.ids
                .iter()
                .enumerate()
                .map(|(node, &id)| (id, node as NodeIx))
                .collect(),
        )
    }

    pub fn id(&self, node: NodeIx) -> i64 {
        self.ids[node as usize]
    }

    pub fn point(&self, node: NodeIx) -> Point {
        Point::new(self.lng[node as usize], self.lat[node as usize])
    }

    /// The out-edges of `node`, in the order they were inserted.
    pub fn out_edges(&self, node: NodeIx) -> impl Iterator<Item = EdgeIx> + use<> {
        self.offsets[node as usize]..self.offsets[node as usize + 1]
    }

    pub fn target(&self, edge: EdgeIx) -> NodeIx {
        self.targets[edge as usize]
    }

    /// Which edges `runtime` may traverse — the matcher's per-edge
    /// accessibility test, evaluated once per way rather than per visit.
    pub fn accessibility(&self, runtime: &<OsmEdgeMetadata as Metadata>::Runtime) -> Vec<bool> {
        let (forward, backward): (Vec<bool>, Vec<bool>) = self
            .meta
            .par_iter()
            .map(|meta| {
                (
                    meta.accessible(runtime, Direction::Outgoing),
                    meta.accessible(runtime, Direction::Incoming),
                )
            })
            .unzip();

        self.way
            .iter()
            .zip(&self.backward)
            .map(|(&way, &against)| match (way as usize, against) {
                (way, false) => forward.get(way).copied().unwrap_or(false),
                (way, true) => backward.get(way).copied().unwrap_or(false),
            })
            .collect()
    }

    /// A spatial index over the edges, holding edge indices rather than
    /// copies of the edges.
    pub fn index_edges(&self) -> RowIndex<EdgeIx> {
        let mut source = vec![0 as NodeIx; self.edges()];
        for node in 0..self.nodes() as NodeIx {
            for edge in self.out_edges(node) {
                source[edge as usize] = node;
            }
        }

        RowIndex::build((0..self.edges() as EdgeIx).collect(), |&edge| {
            envelope_of(
                self.point(source[edge as usize]),
                self.point(self.targets[edge as usize]),
            )
        })
    }

    /// Bytes held by the columns (metadata excluded).
    pub fn footprint(&self) -> usize {
        self.ids.len() * 8
            + self.lng.len() * 8
            + self.lat.len() * 8
            + self.offsets.len() * 4
            + self.targets.len() * 4
            + self.class.len() * 4
            + self.length_cm.len() * 4
            + self.way.len() * 4
            + self.backward.len()
            + self.ways.len() * 8
    }

    /// Serialise: each column as its raw little-endian bytes, the metadata
    /// through postcard.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.footprint() + 64);
        out.extend_from_slice(MAGIC);

        column(&mut out, &self.ids, |v| v.to_le_bytes());
        column(&mut out, &self.lng, |v| v.to_le_bytes());
        column(&mut out, &self.lat, |v| v.to_le_bytes());
        column(&mut out, &self.offsets, |v| v.to_le_bytes());
        column(&mut out, &self.targets, |v| v.to_le_bytes());
        column(&mut out, &self.class, |v| v.to_le_bytes());
        column(&mut out, &self.length_cm, |v| v.to_le_bytes());
        column(&mut out, &self.way, |v| v.to_le_bytes());
        column(&mut out, &self.backward, |v| [*v as u8]);
        column(&mut out, &self.ways, |v| v.to_le_bytes());

        let meta = postcard::to_allocvec(&self.meta).expect("metadata serialises");
        out.extend_from_slice(&(meta.len() as u64).to_le_bytes());
        out.extend_from_slice(&meta);
        out
    }

    /// The inverse of [`to_bytes`](Self::to_bytes).
    pub fn from_bytes(mut bytes: &[u8]) -> Result<Self, String> {
        let bytes = &mut bytes;

        if take(bytes, MAGIC.len())? != MAGIC {
            return Err("not a compact graph".to_string());
        }

        let graph = CompactGraph {
            ids: read(bytes, i64::from_le_bytes)?,
            lng: read(bytes, f64::from_le_bytes)?,
            lat: read(bytes, f64::from_le_bytes)?,
            offsets: read(bytes, u32::from_le_bytes)?,
            targets: read(bytes, u32::from_le_bytes)?,
            class: read(bytes, u32::from_le_bytes)?,
            length_cm: read(bytes, u32::from_le_bytes)?,
            way: read(bytes, u32::from_le_bytes)?,
            backward: read(bytes, |[byte]: [u8; 1]| byte != 0)?,
            ways: read(bytes, i64::from_le_bytes)?,
            meta: {
                let len = take(bytes, 8)?;
                let len = u64::from_le_bytes(len.try_into().expect("8 bytes")) as usize;
                postcard::from_bytes(take(bytes, len)?).map_err(|e| e.to_string())?
            },
        };

        if !bytes.is_empty() {
            return Err("trailing bytes after graph".to_string());
        }

        graph.validate()?;
        Ok(graph)
    }

    /// Check that the columns describe a graph: every index in range, every
    /// per-edge and per-node column the same length. Bytes from disk are not
    /// to be trusted to.
    fn validate(&self) -> Result<(), String> {
        let (nodes, edges) = (self.ids.len(), self.targets.len());
        let check = |holds: bool, what: &str| holds.then_some(()).ok_or(format!("graph: {what}"));

        check(
            self.lng.len() == nodes && self.lat.len() == nodes,
            "position columns",
        )?;
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

fn column<T, const N: usize>(out: &mut Vec<u8>, values: &[T], bytes: impl Fn(&T) -> [u8; N]) {
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    out.extend(values.iter().flat_map(bytes));
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], String> {
    if bytes.len() < len {
        return Err("truncated graph".to_string());
    }

    let (head, tail) = bytes.split_at(len);
    *bytes = tail;
    Ok(head)
}

fn read<T, const N: usize>(
    bytes: &mut &[u8],
    value: impl Fn([u8; N]) -> T,
) -> Result<Vec<T>, String> {
    let len = take(bytes, 8)?;
    let len = u64::from_le_bytes(len.try_into().expect("8 bytes")) as usize;

    let size = len.checked_mul(N).ok_or("graph column too long")?;

    Ok(take(bytes, size)?
        .chunks_exact(N)
        .map(|chunk| value(chunk.try_into().expect("chunk of N")))
        .collect())
}

/// Interleave the bits of a quantised position: nodes close on the map sort
/// close together.
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

/// OSM id to node, for translating at the boundary.
pub struct Lookup(FxHashMap<i64, NodeIx>);

impl Lookup {
    pub fn node(&self, id: OsmEntryId) -> Option<NodeIx> {
        self.0.get(&id.identifier()).copied()
    }
}

/// A path cost as the matcher accumulates it: a running road-class average
/// (kept as its sum and count) and the distance so far.
#[derive(Clone, Copy, Debug, Default)]
struct Cost {
    class_sum: u32,
    edges: u32,
    distance_cm: u32,
}

impl Cost {
    #[inline]
    fn then(self, class: u32, length_cm: u32) -> Self {
        Cost {
            class_sum: self.class_sum + class,
            edges: self.edges + 1,
            distance_cm: self.distance_cm + length_cm,
        }
    }

    /// The ordering key: mean road class, squared, times distance.
    #[inline]
    fn key(self) -> u32 {
        let class = self.class_sum.checked_div(self.edges).unwrap_or(0);

        (f64::from(class).powi(2) * f64::from(self.distance_cm)) as u32
    }
}

#[derive(Clone, Copy, Debug)]
struct Label {
    node: NodeIx,
    /// The label this one was reached from; itself, for the root.
    parent: u32,
    cost: Cost,
    key: u32,
}

/// A frontier entry, ordered by key alone: smallest first.
#[derive(Debug)]
struct Open {
    key: u32,
    cost: Cost,
    label: u32,
}

impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Open {}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

/// The matcher's bounded Dijkstra, over a [`CompactGraph`].
///
/// One `Search` is a reusable workspace: per-node state lives in arrays sized
/// to the graph and is invalidated by bumping a generation counter, so a
/// search touches only the nodes it reaches and, once warm, allocates
/// nothing.
#[derive(Debug, Default)]
pub struct Search {
    /// Generation each node was last discovered in.
    stamp: Vec<u32>,
    /// Each discovered node's label.
    slot: Vec<u32>,
    generation: u32,

    labels: Vec<Label>,
    seen: Vec<bool>,
    open: BinaryHeap<Open>,
    settled: Vec<(NodeIx, NodeIx)>,
}

impl Search {
    pub fn new(graph: &CompactGraph) -> Self {
        Search {
            stamp: vec![0; graph.nodes()],
            slot: vec![0; graph.nodes()],
            ..Search::default()
        }
    }

    /// Every node reachable from `root` before the search first settles one
    /// at `reach` or beyond, as `(node, parent)` in the order settled. The
    /// root's parent is [`NO_NODE`].
    ///
    /// `passable` is [`CompactGraph::accessibility`] for the runtime.
    pub fn reach(
        &mut self,
        graph: &CompactGraph,
        passable: &[bool],
        root: NodeIx,
        reach: Length,
    ) -> &[(NodeIx, NodeIx)] {
        self.advance();
        self.labels.clear();
        self.seen.clear();
        self.open.clear();
        self.settled.clear();

        self.discover(root, 0, Cost::default());

        while let Some(Open { cost, label, .. }) = self.open.pop() {
            if core::mem::replace(&mut self.seen[label as usize], true) {
                continue;
            }

            let Label {
                node,
                parent,
                cost: best,
                ..
            } = self.labels[label as usize];

            // Written as the reference search writes it, so that a reach which
            // compares with nothing (NaN) ends the search here too.
            let within = Length::new::<centimeter>(f64::from(best.distance_cm)) < reach;
            if !within {
                break;
            }

            let from = if parent == label {
                NO_NODE
            } else {
                self.labels[parent as usize].node
            };
            self.settled.push((node, from));

            for edge in graph.out_edges(node) {
                let edge = edge as usize;
                if !passable[edge] {
                    continue;
                }

                let next = cost.then(graph.class[edge], graph.length_cm[edge]);
                self.relax(graph.targets[edge], label, next);
            }
        }

        &self.settled
    }

    /// The least-cost route from `from` to `to` by summed road class — what
    /// `OsmNetwork::route_nodes` computes — as its cost and its nodes.
    ///
    /// Stops as soon as `to` is settled, and shares the workspace with
    /// [`reach`](Self::reach).
    pub fn route(
        &mut self,
        graph: &CompactGraph,
        from: NodeIx,
        to: NodeIx,
    ) -> Option<(u32, Vec<NodeIx>)> {
        self.advance();
        self.labels.clear();
        self.seen.clear();
        self.open.clear();

        let cost = |total: u32| Cost {
            class_sum: total,
            ..Cost::default()
        };

        self.stamp[from as usize] = self.generation;
        self.slot[from as usize] = 0;
        self.labels.push(Label {
            node: from,
            parent: 0,
            cost: cost(0),
            key: 0,
        });
        self.seen.push(false);
        self.open.push(Open {
            key: 0,
            cost: cost(0),
            label: 0,
        });

        while let Some(Open { key, label, .. }) = self.open.pop() {
            if core::mem::replace(&mut self.seen[label as usize], true) {
                continue;
            }

            let node = self.labels[label as usize].node;
            if node == to {
                let mut path = vec![node];
                let mut cursor = label;
                while self.labels[cursor as usize].parent != cursor {
                    cursor = self.labels[cursor as usize].parent;
                    path.push(self.labels[cursor as usize].node);
                }
                path.reverse();
                return Some((key, path));
            }

            for edge in graph.out_edges(node) {
                let edge = edge as usize;
                let (next, total) = (graph.targets[edge], key + graph.class[edge]);

                if self.stamp[next as usize] != self.generation {
                    self.stamp[next as usize] = self.generation;
                    self.slot[next as usize] = self.labels.len() as u32;
                    self.labels.push(Label {
                        node: next,
                        parent: label,
                        cost: cost(total),
                        key: total,
                    });
                    self.seen.push(false);
                } else {
                    let known = &mut self.labels[self.slot[next as usize] as usize];
                    if known.key <= total {
                        continue;
                    }
                    known.parent = label;
                    known.key = total;
                }

                self.open.push(Open {
                    key: total,
                    cost: cost(total),
                    label: self.slot[next as usize],
                });
            }
        }

        None
    }

    /// Begin a new search: nothing discovered before counts.
    fn advance(&mut self) {
        self.generation = self.generation.wrapping_add(1);

        // Back at a generation an old stamp may still carry: forget them all.
        if self.generation == 0 {
            self.stamp.fill(0);
            self.generation = 1;
        }
    }

    /// First sight of `node`.
    fn discover(&mut self, node: NodeIx, parent: u32, cost: Cost) {
        let label = self.labels.len() as u32;
        let parent = if self.labels.is_empty() {
            label
        } else {
            parent
        };
        let key = cost.key();

        self.stamp[node as usize] = self.generation;
        self.slot[node as usize] = label;
        self.labels.push(Label {
            node,
            parent,
            cost,
            key,
        });
        self.seen.push(false);
        self.open.push(Open { key, cost, label });
    }

    /// Offer `node` a route through `parent` at `cost`.
    #[inline]
    fn relax(&mut self, node: NodeIx, parent: u32, cost: Cost) {
        if self.stamp[node as usize] != self.generation {
            return self.discover(node, parent, cost);
        }

        let label = self.slot[node as usize];
        let known = &mut self.labels[label as usize];
        let key = cost.key();

        if known.key > key {
            known.parent = parent;
            known.cost = cost;
            known.key = key;
            self.open.push(Open { key, cost, label });
        }
    }
}
