//! `MultiShardNetwork`: a [`DataPlane`] aggregating multiple shards.
//!
//! Represents a unified network composed of multiple shards, based on
//! a particular sharding strategy and cell window.
//!

mod network;

use core::hash::BuildHasherDefault;
use std::sync::Arc;

use petgraph::prelude::DiGraphMap;
use rstar::RTree;
use rustc_hash::{FxHashMap, FxHashSet, FxHasher};

use routers_network::{DirectionAwareEdgeId, Edge, Entry, Metadata, Node, edge::Weight};

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
