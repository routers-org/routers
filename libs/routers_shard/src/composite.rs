//! `MultiShardNetwork`: a [`DataPlane`] aggregating multiple shards.
//!
//! A single shard exposes only the data within its own cell (plus its
//! halo). Trips that cross shard boundaries can't be routed against one
//! alone. [`MultiShardNetwork`] composes a set of currently-loaded
//! shards — typically the 9-cell window of a
//! [`ShardWindow`](crate::ShardWindow) — into one unified network so the
//! matcher and router see the union.
//!
//! The composite is built eagerly at construction time:
//!
//! - A unified `DiGraphMap` is materialised so `petgraph::algo::astar`
//!   can run across shard boundaries with no special-case logic.
//! - Node and edge `RTree`s are bulk-loaded so spatial lookups (nearest
//!   node, edges-in-box) span every loaded shard.
//! - Per-way metadata is *not* copied; lookups walk the shard list and
//!   short-circuit on the first hit. Cheap enough at typical window
//!   sizes (≤ 9 shards × O(1) `HashMap::get`).
//!
//! Rebuild cost: roughly `O(N log N)` for the rtree bulk-loads, dominated
//! by edge count. For Sydney precision-5 cells at ~10k edges/shard ×
//! 9 shards = ~90k edges, this is ~30 ms on a modern CPU. Build a new
//! composite whenever the loaded set changes; reuse it otherwise.

use core::fmt::Debug;
use core::hash::BuildHasherDefault;
use std::sync::Arc;

use geo::Point;
use petgraph::prelude::DiGraphMap;
use rstar::{AABB, RTree};
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use routers_network::{
    DataPlane, DirectionAwareEdgeId, Discovery, Edge, Entry, Metadata, Node, Route, Scan,
    edge::Weight, network::GraphEdge,
};

use crate::network::ShardedNetwork;
use crate::strategy::ShardId;

type GraphStructure<E> =
    DiGraphMap<E, (Weight, DirectionAwareEdgeId<E>), BuildHasherDefault<FxHasher>>;

pub struct MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    /// The participating shards, kept alive via `Arc` so references into
    /// `meta`/`hash` from the composite's read methods stay valid.
    shards: Vec<Arc<ShardedNetwork<E, M, S>>>,
    /// Union of every shard's `graph`. Built once at construction; used
    /// by `Route::route_nodes` via `petgraph::algo::astar`.
    graph: GraphStructure<E>,
    /// Union of every shard's `hash` — id → `Node<E>`. Lets
    /// `Discovery::node` resolve identifiers without searching each shard.
    hash: FxHashMap<E, Node<E>>,
    /// Spatial indices covering every loaded shard, so
    /// `Scan::nearest_node` and `Discovery::*_in_box` Just Work across
    /// the window.
    index: RTree<Node<E>>,
    index_edge: RTree<Edge<Node<E>>>,
}

impl<E, M, S> MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    /// An empty composite — zero shards, no data. Useful as a "still
    /// loading" placeholder before the first shard arrives, mirroring
    /// `OsmNetwork::empty`.
    pub fn empty() -> Self {
        Self {
            shards: Vec::new(),
            graph: GraphStructure::new(),
            hash: FxHashMap::default(),
            index: RTree::new(),
            index_edge: RTree::new(),
        }
    }

    /// Compose `shards` into a single network. Iterates every shard once
    /// to build the union graph, the unified node hash and the
    /// composite spatial indices. The two `RTree`s are bulk-loaded in
    /// parallel via `rayon::join`.
    pub fn new(shards: Vec<Arc<ShardedNetwork<E, M, S>>>) -> Self {
        let mut graph: GraphStructure<E> = GraphStructure::new();
        let mut hash: FxHashMap<E, Node<E>> = FxHashMap::default();
        let mut nodes_seen: FxHashSet<E> = FxHashSet::default();
        let mut nodes: Vec<Node<E>> = Vec::new();
        let mut edges_seen: FxHashSet<(E, E, routers_network::Direction)> = FxHashSet::default();
        let mut edges: Vec<Edge<Node<E>>> = Vec::new();

        for shard in &shards {
            // Hash + graph: dedupe by node id and (src, dst, dir). A node
            // appearing in two shards via the halo is recorded once; the
            // (weight, edge_id) values are identical in both shards
            // because they come from the same source way, so the choice
            // of "winner" doesn't matter.
            for (id, node) in &shard.hash {
                if nodes_seen.insert(*id) {
                    nodes.push(*node);
                    hash.insert(*id, *node);
                }
            }
            for (src, dst, &(weight, edge_id)) in shard.graph.all_edges() {
                graph.add_edge(src, dst, (weight, edge_id));
            }

            // Spatial edge index: dedupe by (src, dst, direction). The
            // shard's `index_edge` was bulk-loaded over a `Vec<Edge<Node<E>>>`
            // so iter yields all of them.
            for edge in shard.index_edge.iter() {
                let key = (edge.source.id, edge.target.id, edge.id.direction());
                if edges_seen.insert(key) {
                    edges.push(*edge);
                }
            }
        }

        let (index, index_edge) =
            rayon::join(|| RTree::bulk_load(nodes), || RTree::bulk_load(edges));

        Self {
            shards,
            graph,
            hash,
            index,
            index_edge,
        }
    }

    /// Number of shards composed into this network.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn num_nodes(&self) -> usize {
        self.hash.len()
    }

    pub fn num_edges(&self) -> usize {
        self.graph.edge_count()
    }
}

impl<E, M, S> Debug for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "MultiShardNetwork({} shards, {} nodes, {} edges)",
            self.shards.len(),
            self.num_nodes(),
            self.num_edges()
        )
    }
}

impl<E, M, S> DataPlane for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    type Entry = E;
    type Meta = M;

    fn metadata(&self, id: &E) -> Option<&M> {
        // Walk shards on each call rather than copying every metadata
        // entry into the composite — the latter would roughly double
        // memory use. At typical window sizes (≤ 9 shards) this is a
        // handful of `HashMap::get` calls per lookup.
        self.shards.iter().find_map(|s| s.meta.get(id))
    }

    fn point(&self, id: &E) -> Option<Point> {
        self.hash.get(id).map(|n| n.position)
    }

    fn edges_outof<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Outgoing)
                .map(|(s, t, &data)| (s, t, data)),
        )
    }

    fn edges_into<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(
            self.graph
                .edges_directed(id, petgraph::Direction::Incoming)
                .map(|(s, t, &data)| (s, t, data)),
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

impl<E, M, S> Discovery<E> for MultiShardNetwork<E, M, S>
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

impl<E, M, S> Scan<E> for MultiShardNetwork<E, M, S>
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

impl<E, M, S> Route<E> for MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn route_nodes(&self, start: E, finish: E) -> Option<(Weight, Vec<Node<E>>)> {
        let (cost, path) = petgraph::algo::astar(
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
        Some((cost, route))
    }
}
