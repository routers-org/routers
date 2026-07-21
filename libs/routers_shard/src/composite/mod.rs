//! `MultiShardNetwork`: a [`DataPlane`] aggregating multiple shards.
//!
//! Represents a unified network composed of multiple shards, based on
//! a particular sharding strategy and cell window.
//!

mod network;

use core::hash::BuildHasherDefault;
use std::sync::Arc;

use petgraph::prelude::DiGraphMap;
use rustc_hash::FxHasher;

use routers_network::{DirectionAwareEdgeId, Entry, Metadata, edge::Weight};

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
}

impl<E, M, S> MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    pub fn new(shards: Vec<Arc<ShardedNetwork<E, M, S>>>) -> Self {
        let mut graph: GraphStructure<E> = GraphStructure::new();

        for shard in &shards {
            // Nodes first so isolated nodes (no incident edges) still count.
            for node in shard.graph.nodes() {
                graph.add_node(node);
            }

            for (src, dst, &(weight, edge_id)) in shard.graph.all_edges() {
                graph.add_edge(src, dst, (weight, edge_id));
            }
        }

        Self { shards, graph }
    }

    /// Number of shards composed into this network.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn num_nodes(&self) -> usize {
        self.graph.node_count()
    }

    pub fn num_edges(&self) -> usize {
        self.graph.edge_count()
    }
}
