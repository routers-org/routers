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
    shards: Vec<Arc<ShardedNetwork<E, M, S>>>,

    graph: GraphStructure<E>,
    hash: FxHashMap<E, Node<E>>,

    index: RTree<Node<E>>,
    index_edge: RTree<Edge<Node<E>>>,
}

impl<E, M, S> MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    pub fn new(shards: Vec<Arc<ShardedNetwork<E, M, S>>>) -> Self {
        let mut graph: GraphStructure<E> = GraphStructure::new();
        let mut hash: FxHashMap<E, Node<E>> = FxHashMap::default();

        let mut nodes_seen: FxHashSet<E> = FxHashSet::default();
        let mut nodes: Vec<Node<E>> = Vec::new();

        let mut edges_seen: FxHashSet<(E, E, routers_network::Direction)> = FxHashSet::default();
        let mut edges: Vec<Edge<Node<E>>> = Vec::new();

        for shard in &shards {
            for (id, node) in &shard.hash {
                if nodes_seen.insert(*id) {
                    nodes.push(*node);
                    hash.insert(*id, *node);
                }
            }

            for (src, dst, &(weight, edge_id)) in shard.graph.all_edges() {
                graph.add_edge(src, dst, (weight, edge_id));
            }

            for edge in shard.index_edge.iter() {
                if edges_seen.insert((edge.source.id, edge.target.id, edge.id.direction())) {
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
