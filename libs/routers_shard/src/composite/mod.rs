//! `MultiShardNetwork`: a [`DataPlane`](routers_network::DataPlane)
//! aggregating multiple shards.
//!
//! Represents a unified network composed of multiple shards, based on
//! a particular sharding strategy and cell window.

mod network;

use alloc::sync::Arc;

use routers_network::columnar::{Masks, Topology};
use routers_network::{Entry, Metadata};

use crate::network::ShardedNetwork;
use crate::strategy::ShardId;

pub struct MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    shards: Vec<Arc<ShardedNetwork<E, M, S>>>,
    /// The shards' graphs as one, which a route may cross freely. Metadata
    /// and spatial indices stay with the shards.
    topology: Topology<E>,
    passable: Masks<M::Runtime>,
}

impl<E, M, S> MultiShardNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    pub fn new(shards: Vec<Arc<ShardedNetwork<E, M, S>>>) -> Self {
        let topologies = shards
            .iter()
            .map(|shard| shard.graph().topology())
            .collect::<Vec<_>>();

        Self {
            topology: Topology::merge(&topologies),
            passable: Masks::default(),
            shards,
        }
    }

    /// Number of shards composed into this network.
    pub fn shard_count(&self) -> usize {
        self.shards.len()
    }

    pub fn num_nodes(&self) -> usize {
        self.topology.num_nodes()
    }

    pub fn num_edges(&self) -> usize {
        self.topology.num_edges()
    }
}
