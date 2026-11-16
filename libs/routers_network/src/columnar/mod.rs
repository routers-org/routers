//! A routing graph held as flat columns: nodes in Z-order, edges in CSR, with
//! each edge's length and road class beside its target.
//!
//! [`Topology`] is the graph a search walks. [`Graph`] adds edge metadata and
//! spatial indices, and implements the network traits; a network type wraps
//! one and forwards to it with [`columnar_network!`](crate::columnar_network).

mod graph;
mod search;

use alloc::sync::Arc;
use core::ops::Range;
use std::sync::{Mutex, OnceLock, PoisonError};

use geo::{Distance, Haversine, Point};
use rustc_hash::FxHashMap;
use serde::de::DeserializeOwned;
use uom::si::f64::Length;
use uom::si::length::{centimeter, meter};

use crate::edge::Weight;
use crate::network::GraphEdge;
use crate::{Direction, DirectionAwareEdgeId, Edge, Entry, Node};

pub use graph::{Graph, Masks};

#[doc(hidden)]
pub mod __private {
    pub use geo::{Point, Rect};
}
use search::{Csr, NO_NODE, Search};

/// A node of the graph: an index into its node column.
pub type NodeIx = u32;

/// An edge of the graph: an index into its edge columns.
pub type EdgeIx = u32;

/// The way of an edge whose way is not among the graph's.
const NO_WAY: u32 = u32::MAX;

/// Revision of the encoded layout. A saved graph's version should fold this
/// in, so that a change here invalidates it.
pub const FORMAT: u64 = 1;

/// One directed edge, before it is laid out.
#[derive(Clone, Copy, Debug)]
pub struct RawEdge<E: Entry> {
    pub source: E,
    pub target: E,
    pub class: Weight,
    pub way: DirectionAwareEdgeId<E>,
}

/// The graph a search walks: nodes, their out-edges, and the way each edge
/// belongs to.
pub struct Topology<E: Entry> {
    nodes: Vec<Node<E>>,
    csr: Csr,
    /// Per edge: the way it belongs to, as an index into `ways`.
    way: Vec<u32>,
    /// Per edge: whether it runs against its way's direction.
    backward: Vec<bool>,
    ways: Vec<E>,

    // Derived from the columns above.
    node_of: FxHashMap<E, NodeIx>,
    way_of: FxHashMap<E, u32>,
    /// Per edge: the node it leaves.
    sources: Vec<NodeIx>,
    /// In-edges per node, built the first time they are asked for.
    incoming: OnceLock<Vec<Vec<EdgeIx>>>,
    /// Idle search workspaces, one per thread that has searched.
    searches: Mutex<Vec<Search>>,
}

impl<E: Entry> Default for Topology<E> {
    fn default() -> Self {
        Self::assemble(Vec::new(), Vec::new(), Vec::new())
    }
}

impl<E: Entry> Topology<E> {
    /// Lay `nodes` and `edges` out as columns. `ways` must be sorted and
    /// unique; an edge of any other way has no way.
    ///
    /// `edges` are in insertion order: a repeated `(source, target)` keeps its
    /// first position and takes its last data. An edge with an endpoint not
    /// among `nodes` is dropped.
    pub fn assemble(mut nodes: Vec<Node<E>>, edges: Vec<RawEdge<E>>, ways: Vec<E>) -> Self {
        sort_by_key(&mut nodes, |node| node.id);
        nodes.dedup_by_key(|node| node.id);

        let mut placed = map(&nodes, |node| (morton(node.position), *node));
        sort_by_key(&mut placed, |&(code, node)| (code, node.id));

        // `nodes` is sorted by id: its inverse permutation onto the Z-order
        // numbering turns an id into a node by binary search.
        let ordered = placed.into_iter().map(|(_, node)| node).collect::<Vec<_>>();
        let mut by_id = (0..ordered.len() as NodeIx).collect::<Vec<_>>();
        sort_by_key(&mut by_id, |&node| ordered[node as usize].id);
        let node_of = |id: E| {
            let found = nodes.binary_search_by_key(&id, |node| node.id).ok()?;
            Some(by_id[found])
        };
        let way_of = |id: E| ways.binary_search(&id).map_or(NO_WAY, |found| found as u32);

        let resolved = map(&edges, |edge| {
            Some((node_of(edge.source)?, node_of(edge.target)?, *edge))
        })
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
        drop(edges);
        let nodes = ordered;

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
        let mut csr = Csr {
            offsets: vec![0],
            targets: Vec::with_capacity(resolved.len()),
            class: Vec::with_capacity(resolved.len()),
            length_cm: Vec::new(),
        };
        let mut way = Vec::with_capacity(resolved.len());
        let mut backward = Vec::with_capacity(resolved.len());

        for node in 0..nodes.len() {
            let start = csr.targets.len();

            for &slot in &slots[offsets[node] as usize..offsets[node + 1] as usize] {
                let (_, target, edge) = resolved[slot as usize];
                let against = edge.way.direction() == Direction::Incoming;

                let repeat = csr.targets[start..].iter().position(|&seen| seen == target);
                match repeat {
                    Some(first) => {
                        csr.class[start + first] = edge.class;
                        way[start + first] = way_of(edge.way.index());
                        backward[start + first] = against;
                    }
                    None => {
                        csr.targets.push(target);
                        csr.class.push(edge.class);
                        way.push(way_of(edge.way.index()));
                        backward.push(against);
                    }
                }
            }

            csr.offsets.push(csr.targets.len() as u32);
        }

        let edges = (0..nodes.len() as NodeIx)
            .flat_map(|node| csr.out_edges(node).map(move |edge| (node, edge)))
            .collect::<Vec<_>>();
        csr.length_cm = map(&edges, |&(node, edge)| {
            let from = nodes[node as usize].position;
            let to = nodes[csr.targets[edge as usize] as usize].position;

            Length::new::<meter>(Haversine.distance(from, to)).get::<centimeter>() as u32
        });

        Self::derive(nodes, csr, way, backward, ways, None)
    }

    /// Several graphs as one. Where they share a node it appears once, and
    /// where they share an edge the last of them has its say.
    ///
    /// Equal to [assembling](Self::assemble) their nodes and edges together,
    /// but for graphs already laid out: nothing is measured again.
    pub fn merge(parts: &[&Topology<E>]) -> Self {
        let mut placed = parts
            .iter()
            .flat_map(|part| &part.nodes)
            .map(|node| (morton(node.position), *node))
            .collect::<Vec<_>>();
        placed.sort_unstable_by_key(|&(code, node)| (code, node.id));

        let mut nodes = Vec::with_capacity(placed.len());
        let mut node_of =
            FxHashMap::with_capacity_and_hasher(placed.len(), rustc_hash::FxBuildHasher);
        for (_, node) in placed {
            if let std::collections::hash_map::Entry::Vacant(slot) = node_of.entry(node.id) {
                slot.insert(nodes.len() as NodeIx);
                nodes.push(node);
            }
        }

        let mut ways = parts
            .iter()
            .flat_map(|part| part.ways.iter().copied())
            .collect::<Vec<_>>();
        ways.sort_unstable();
        ways.dedup();

        // Each part's nodes and ways, as the merged graph numbers them.
        let renumbered = parts
            .iter()
            .map(|part| {
                let nodes = part
                    .nodes
                    .iter()
                    .map(|node| node_of[&node.id])
                    .collect::<Vec<_>>();
                let ways = part
                    .ways
                    .iter()
                    .map(|way| ways.binary_search(way).map_or(NO_WAY, |found| found as u32))
                    .collect::<Vec<_>>();

                (nodes, ways)
            })
            .collect::<Vec<_>>();

        // Counting sort by source, parts in order: stable, so each node's
        // edges stay in insertion order.
        let mut offsets = vec![0u32; nodes.len() + 1];
        for (part, (local, _)) in parts.iter().zip(&renumbered) {
            for &source in &part.sources {
                offsets[local[source as usize] as usize + 1] += 1;
            }
        }
        for node in 0..nodes.len() {
            offsets[node + 1] += offsets[node];
        }

        let total = offsets[nodes.len()] as usize;
        let mut cursor = offsets.clone();
        let mut sorted = vec![(0 as NodeIx, 0 as Weight, 0u32, NO_WAY, false); total];
        for (part, (local, local_ways)) in parts.iter().zip(&renumbered) {
            for edge in 0..part.num_edges() {
                let slot = &mut cursor[local[part.sources[edge] as usize] as usize];
                let way = part.way[edge];

                sorted[*slot as usize] = (
                    local[part.csr.targets[edge] as usize],
                    part.csr.class[edge],
                    part.csr.length_cm[edge],
                    local_ways.get(way as usize).copied().unwrap_or(NO_WAY),
                    part.backward[edge],
                );
                *slot += 1;
            }
        }
        drop(cursor);

        // Emit each node's edges, folding repeats of a target into the first.
        let mut csr = Csr {
            offsets: vec![0],
            targets: Vec::with_capacity(total),
            class: Vec::with_capacity(total),
            length_cm: Vec::with_capacity(total),
        };
        let mut way = Vec::with_capacity(total);
        let mut backward = Vec::with_capacity(total);

        for node in 0..nodes.len() {
            let start = csr.targets.len();

            for &(target, class, length_cm, edge_way, against) in
                &sorted[offsets[node] as usize..offsets[node + 1] as usize]
            {
                let repeat = csr.targets[start..].iter().position(|&seen| seen == target);
                match repeat {
                    Some(first) => {
                        csr.class[start + first] = class;
                        csr.length_cm[start + first] = length_cm;
                        way[start + first] = edge_way;
                        backward[start + first] = against;
                    }
                    None => {
                        csr.targets.push(target);
                        csr.class.push(class);
                        csr.length_cm.push(length_cm);
                        way.push(edge_way);
                        backward.push(against);
                    }
                }
            }

            csr.offsets.push(csr.targets.len() as u32);
        }

        Self::derive(nodes, csr, way, backward, ways, Some(node_of))
    }

    /// Derive the lookups over the columns.
    fn derive(
        nodes: Vec<Node<E>>,
        csr: Csr,
        way: Vec<u32>,
        backward: Vec<bool>,
        ways: Vec<E>,
        node_of: Option<FxHashMap<E, NodeIx>>,
    ) -> Self {
        let node_of = node_of.unwrap_or_else(|| {
            nodes
                .iter()
                .enumerate()
                .map(|(node, entry)| (entry.id, node as NodeIx))
                .collect()
        });
        let way_of = ways
            .iter()
            .enumerate()
            .map(|(way, &id)| (id, way as u32))
            .collect();

        let mut sources = vec![0 as NodeIx; csr.targets.len()];
        for node in 0..nodes.len() as NodeIx {
            for edge in csr.out_edges(node) {
                sources[edge as usize] = node;
            }
        }

        Topology {
            nodes,
            csr,
            way,
            backward,
            ways,
            node_of,
            way_of,
            sources,
            incoming: OnceLock::new(),
            searches: Mutex::new(Vec::new()),
        }
    }

    /// Check that the columns describe a graph.
    fn validate(
        nodes: usize,
        csr: &Csr,
        way: &[u32],
        backward: &[bool],
        ways: usize,
    ) -> Result<(), String> {
        let edges = csr.targets.len();
        let check = |holds: bool, what: &str| holds.then_some(()).ok_or(format!("graph: {what}"));

        check(csr.offsets.len() == nodes + 1, "offset column")?;
        check(csr.offsets.first() == Some(&0), "offsets start")?;
        check(csr.offsets.is_sorted(), "offsets order")?;
        check(
            csr.offsets.last().map(|&end| end as usize) == Some(edges),
            "offsets end",
        )?;
        check(
            [
                csr.class.len(),
                csr.length_cm.len(),
                way.len(),
                backward.len(),
            ]
            .iter()
            .all(|&len| len == edges),
            "edge columns",
        )?;
        check(
            csr.targets.iter().all(|&target| (target as usize) < nodes),
            "edge targets",
        )?;
        check(
            way.iter()
                .all(|&way| way == NO_WAY || (way as usize) < ways),
            "edge ways",
        )
    }

    /// Append the columns to `out`.
    ///
    /// Integers are written at variable width, and each edge's target as
    /// its distance from its source: nodes near on the map are near in
    /// number, so most fit a byte or two.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), String> {
        let ids = self.nodes.iter().map(|node| node.id).collect::<Vec<_>>();
        let degrees = (0..self.num_nodes() as NodeIx)
            .map(|node| self.out_edges(node).len() as u32)
            .collect::<Vec<_>>();
        let strides = self
            .csr
            .targets
            .iter()
            .zip(&self.sources)
            .map(|(&target, &source)| target.wrapping_sub(source) as i32)
            .collect::<Vec<_>>();
        let backward = self
            .backward
            .chunks(8)
            .map(|bits| {
                bits.iter()
                    .enumerate()
                    .fold(0u8, |byte, (bit, &set)| byte | (u8::from(set) << bit))
            })
            .collect::<Vec<_>>();

        blob(out, &ids)?;
        write(out, &self.nodes, |node| {
            let mut raw = [0; 16];
            raw[..8].copy_from_slice(&node.position.x().to_le_bytes());
            raw[8..].copy_from_slice(&node.position.y().to_le_bytes());
            raw
        });
        blob(out, &degrees)?;
        blob(out, &strides)?;
        blob(out, &self.csr.class)?;
        blob(out, &self.csr.length_cm)?;
        blob(out, &self.way)?;
        blob(out, &backward)?;
        blob(out, &self.ways)
    }

    /// Read columns [encoded](Self::encode) at the head of `bytes`.
    pub fn decode(bytes: &mut &[u8]) -> Result<Self, String>
    where
        E: DeserializeOwned,
    {
        let ids: Vec<E> = unblob(bytes)?;
        let positions = read(bytes, |raw: [u8; 16]| {
            let word = |at: usize| raw[at..at + 8].try_into().expect("8 bytes");
            Point::new(f64::from_le_bytes(word(0)), f64::from_le_bytes(word(8)))
        })?;
        if ids.len() != positions.len() {
            return Err("graph: node columns".to_string());
        }

        let nodes = positions
            .into_iter()
            .zip(ids)
            .map(|(position, id)| Node::new(position, id))
            .collect::<Vec<_>>();

        let degrees: Vec<u32> = unblob(bytes)?;
        let strides: Vec<i32> = unblob(bytes)?;

        let mut offsets = Vec::with_capacity(degrees.len() + 1);
        let mut targets = Vec::with_capacity(strides.len());
        let (mut end, mut strides) = (0u32, strides.into_iter());
        offsets.push(end);

        for (source, &degree) in degrees.iter().enumerate() {
            end = end.checked_add(degree).ok_or("graph: offsets end")?;
            offsets.push(end);

            targets.extend(
                strides
                    .by_ref()
                    .take(degree as usize)
                    .map(|stride| (source as u32).wrapping_add(stride as u32)),
            );
        }
        if strides.next().is_some() {
            return Err("graph: offsets end".to_string());
        }

        let csr = Csr {
            offsets,
            targets,
            class: unblob(bytes)?,
            length_cm: unblob(bytes)?,
        };
        let way: Vec<u32> = unblob(bytes)?;
        let packed: Vec<u8> = unblob(bytes)?;
        if packed.len() != way.len().div_ceil(8) {
            return Err("graph: edge columns".to_string());
        }
        let backward = (0..way.len())
            .map(|edge| packed[edge / 8] >> (edge % 8) & 1 == 1)
            .collect::<Vec<_>>();
        let ways: Vec<E> = unblob(bytes)?;

        Self::validate(nodes.len(), &csr, &way, &backward, ways.len())?;
        Ok(Self::derive(nodes, csr, way, backward, ways, None))
    }

    pub fn num_nodes(&self) -> usize {
        self.nodes.len()
    }

    pub fn num_edges(&self) -> usize {
        self.csr.targets.len()
    }

    /// Every node, in the graph's own order.
    pub fn nodes(&self) -> &[Node<E>] {
        &self.nodes
    }

    /// The ways edges belong to, sorted.
    pub fn ways(&self) -> &[E] {
        &self.ways
    }

    pub fn node(&self, id: &E) -> Option<&Node<E>> {
        self.node_of.get(id).map(|&node| &self.nodes[node as usize])
    }

    /// Where `way` sits among [`ways`](Self::ways).
    pub fn way_index(&self, way: &E) -> Option<usize> {
        self.way_of.get(way).map(|&way| way as usize)
    }

    /// The out-edges of `node`, in the order they were inserted.
    #[inline]
    pub fn out_edges(&self, node: NodeIx) -> Range<EdgeIx> {
        self.csr.out_edges(node)
    }

    /// The nodes `edge` runs between.
    #[inline]
    pub fn endpoints(&self, edge: EdgeIx) -> (&Node<E>, &Node<E>) {
        (
            &self.nodes[self.sources[edge as usize] as usize],
            &self.nodes[self.csr.targets[edge as usize] as usize],
        )
    }

    #[inline]
    pub fn class(&self, edge: EdgeIx) -> Weight {
        self.csr.class[edge as usize]
    }

    /// The way `edge` belongs to, and the direction it runs along it.
    pub fn way(&self, edge: EdgeIx) -> DirectionAwareEdgeId<E> {
        let way = self.way[edge as usize] as usize;
        let id = self.ways.get(way).copied().unwrap_or_default();

        DirectionAwareEdgeId::new(id).with_direction(if self.backward[edge as usize] {
            Direction::Incoming
        } else {
            Direction::Outgoing
        })
    }

    pub fn graph_edge(&self, edge: EdgeIx) -> GraphEdge<E> {
        let (source, target) = self.endpoints(edge);

        (source.id, target.id, (self.class(edge), self.way(edge)))
    }

    /// Every directed edge, grouped by the node it leaves.
    pub fn edges(&self) -> impl Iterator<Item = GraphEdge<E>> + '_ {
        (0..self.num_edges() as EdgeIx).map(|edge| self.graph_edge(edge))
    }

    pub fn edges_outof(&self, id: E) -> impl Iterator<Item = GraphEdge<E>> + '_ {
        let edges = self.node_of.get(&id).map(|&node| self.out_edges(node));

        edges
            .into_iter()
            .flatten()
            .map(|edge| self.graph_edge(edge))
    }

    pub fn edges_into(&self, id: E) -> impl Iterator<Item = GraphEdge<E>> + '_ {
        let incoming = self.incoming.get_or_init(|| {
            let mut incoming = vec![Vec::new(); self.num_nodes()];
            for (edge, &target) in self.csr.targets.iter().enumerate() {
                incoming[target as usize].push(edge as EdgeIx);
            }
            incoming
        });
        let edges = self.node_of.get(&id).map(|&node| &incoming[node as usize]);

        edges
            .into_iter()
            .flatten()
            .map(|&edge| self.graph_edge(edge))
    }

    pub fn edge(&self, source: &E, target: &E) -> Option<Edge<E>> {
        let (from, to) = (*self.node_of.get(source)?, *self.node_of.get(target)?);
        let edge = self
            .out_edges(from)
            .find(|&edge| self.csr.targets[edge as usize] == to)?;

        Some(Edge {
            source: *source,
            target: *target,
            weight: self.class(edge),
            id: self.way(edge),
        })
    }

    pub fn fatten(&self, edge: &Edge<E>) -> Option<Edge<Node<E>>> {
        Some(Edge {
            source: *self.node(&edge.source)?,
            target: *self.node(&edge.target)?,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), edge.id.index())),
            weight: edge.weight,
        })
    }

    /// `edge` with its endpoints in full, as a spatial query answers.
    pub fn fat_edge(&self, edge: EdgeIx) -> Edge<Node<E>> {
        let (source, target) = self.endpoints(edge);
        let way = self.way(edge);

        Edge {
            source: *source,
            target: *target,
            id: DirectionAwareEdgeId::new(Node::new(Point::new(0., 0.), way.index()))
                .with_direction(way.direction()),
            weight: self.class(edge),
        }
    }

    /// Which edges may be traversed, given which ways may be in each
    /// direction. An edge with no way may not.
    pub fn passable(&self, allowed: impl Fn(usize, Direction) -> bool) -> Arc<[bool]> {
        let along = |direction| {
            (0..self.ways.len())
                .map(|way| allowed(way, direction))
                .collect::<Vec<_>>()
        };
        let (forward, backward) = (along(Direction::Outgoing), along(Direction::Incoming));

        self.way
            .iter()
            .zip(&self.backward)
            .map(|(&way, &against)| {
                let allowed = if against { &backward } else { &forward };
                allowed.get(way as usize).copied().unwrap_or(false)
            })
            .collect()
    }

    /// The matcher's bounded reachability search from `root` over the
    /// `passable` edges; see [`DataPlane::reach`](crate::DataPlane::reach).
    pub fn reach(&self, root: &E, reach: f64, passable: &[bool]) -> Option<Vec<(E, E)>> {
        let root = *self.node_of.get(root)?;
        let nodes = &self.nodes;

        Some(self.searching(|search| {
            search
                .reach(&self.csr, passable, root, Length::new::<meter>(reach))
                .iter()
                .map(|&(node, parent)| {
                    let parent = match parent {
                        NO_NODE => E::default(),
                        parent => nodes[parent as usize].id,
                    };
                    (nodes[node as usize].id, parent)
                })
                .collect()
        }))
    }

    /// The least-cost route between two nodes by summed road class.
    pub fn route_nodes(&self, start: E, finish: E) -> Option<(Weight, Vec<Node<E>>)> {
        let (from, to) = (*self.node_of.get(&start)?, *self.node_of.get(&finish)?);
        let (cost, path) = self.searching(|search| search.route(&self.csr, from, to))?;

        Some((
            cost,
            path.into_iter()
                .map(|node| self.nodes[node as usize])
                .collect(),
        ))
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
}

/// Interleave the bits of a quantised position (Z-order).
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

/// Sort by key, on the thread pool where there is one.
fn sort_by_key<T: Send, K: Ord>(items: &mut [T], key: impl Fn(&T) -> K + Sync) {
    #[cfg(feature = "parallel")]
    rayon::slice::ParallelSliceMut::par_sort_unstable_by_key(items, key);
    #[cfg(not(feature = "parallel"))]
    items.sort_unstable_by_key(key);
}

/// Map in order, on the thread pool where there is one.
fn map<T: Sync, U: Send>(items: &[T], with: impl Fn(&T) -> U + Sync + Send) -> Vec<U> {
    #[cfg(feature = "parallel")]
    {
        use rayon::prelude::*;
        items.par_iter().map(with).collect()
    }
    #[cfg(not(feature = "parallel"))]
    items.iter().map(with).collect()
}

/// Append a column: its length, then each value's little-endian bytes.
fn write<T, const N: usize>(out: &mut Vec<u8>, values: &[T], bytes: impl Fn(&T) -> [u8; N]) {
    out.extend_from_slice(&(values.len() as u64).to_le_bytes());
    out.extend(values.iter().flat_map(bytes));
}

fn take<'a>(bytes: &mut &'a [u8], len: usize) -> Result<&'a [u8], String> {
    let (head, tail) = bytes.split_at_checked(len).ok_or("truncated graph")?;
    *bytes = tail;
    Ok(head)
}

fn length(bytes: &mut &[u8]) -> Result<usize, String> {
    Ok(u64::from_le_bytes(take(bytes, 8)?.try_into().expect("8 bytes")) as usize)
}

/// Read a column [written](write) at the head of `bytes`.
fn read<T, const N: usize>(
    bytes: &mut &[u8],
    value: impl Fn([u8; N]) -> T,
) -> Result<Vec<T>, String> {
    let size = length(bytes)?
        .checked_mul(N)
        .ok_or("graph column too long")?;

    Ok(take(bytes, size)?
        .as_chunks::<N>()
        .0
        .iter()
        .map(|&chunk| value(chunk))
        .collect())
}

/// Append a serialised value, behind its length in bytes.
pub fn blob<T: serde::Serialize + ?Sized>(out: &mut Vec<u8>, value: &T) -> Result<(), String> {
    let bytes =
        postcard::to_allocvec(value).map_err(|e| format!("failed to serialise value: {e}"))?;

    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(&bytes);
    Ok(())
}

/// Read a value [written](blob) at the head of `bytes`.
pub fn unblob<T: DeserializeOwned>(bytes: &mut &[u8]) -> Result<T, String> {
    let size = length(bytes)?;

    postcard::from_bytes(take(bytes, size)?).map_err(|e| e.to_string())
}
