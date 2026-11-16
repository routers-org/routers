//! Shards cut straight from a columnar graph.
//!
//! [`Partition`](crate::Partition) deals any source into shards, and to do
//! so holds every position and every edge of it at once. A source that is
//! already a [`Graph`] needs none of that: which nodes a shard admits is one
//! pass over the node column, and each shard is then a part of the source's
//! own columns, cut out on its own — in whatever order, on whatever thread,
//! holding nothing but itself.

use alloc::collections::BinaryHeap;
use core::cmp::Reverse;

use rustc_hash::{FxHashMap, FxHashSet};

use routers_network::columnar::{Graph, NodeIx};
use routers_network::{Entry, Metadata};

use crate::network::ShardedNetwork;
use crate::partition::Dealer;
use crate::selection::SelectionMode;
use crate::strategy::{ShardId, ShardingStrategy};

/// The shards of a columnar graph, each cut on request. Yields exactly the
/// shards [`Partition`](crate::Partition) would deal the same graph into.
pub struct Carving<'a, E: Entry, M: Metadata, S: ShardId> {
    graph: &'a Graph<E, M>,
    /// Each shard with the nodes its padding admits, in the graph's order.
    cells: Vec<(S, Vec<NodeIx>)>,
}

impl<'a, E, M, S> Carving<'a, E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    /// Work out which nodes of `graph` each shard admits.
    pub fn new<St>(graph: &'a Graph<E, M>, strategy: &St, padding_distance: f64) -> Self
    where
        St: ShardingStrategy<Id = S>,
    {
        let mut dealer = Dealer {
            strategy,
            mode: SelectionMode::OwnedAndPadded { padding_distance },
            neighbourhoods: FxHashMap::default(),
        };

        // Whether a cell owns any node, and the nodes it admits.
        let mut admitted: FxHashMap<S, (bool, Vec<NodeIx>)> = FxHashMap::default();
        let mut cells = Vec::with_capacity(9);

        for (index, node) in graph.nodes().enumerate() {
            let owner = dealer.cells_of(node.position, &mut cells);

            for cell in &cells {
                let (owned, nodes) = admitted.entry(*cell).or_default();
                *owned |= *cell == owner;
                nodes.push(index as NodeIx);
            }
        }

        // Padding alone does not make a shard.
        let mut cells = admitted
            .into_iter()
            .filter(|(_, (owned, _))| *owned)
            .map(|(cell, (_, nodes))| (cell, nodes))
            .collect::<Vec<_>>();
        cells.sort_by_key(|(cell, _)| *cell);

        Carving { graph, cells }
    }

    /// Drop every shard whose id fails `keep`.
    pub fn retain(&mut self, mut keep: impl FnMut(&S) -> bool) {
        self.cells.retain(|(cell, _)| keep(cell));
    }

    /// Shards to be cut.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The id of shard `at`, in ascending order of id.
    pub fn id(&self, at: usize) -> S {
        self.cells[at].0
    }

    /// Cut shard `at`, without spatial indices.
    ///
    /// A shard holds the nodes it admits, their out-edges, and the far end
    /// of any of those edges that it did not admit. Such a node "rides
    /// along", and brings its own out-edges with it only if it comes later
    /// in the graph than the node that pulled it in: that is what a single
    /// pass over the edges in order — which is how a shard is otherwise
    /// built — would have seen.
    pub fn shard(&self, at: usize) -> ShardedNetwork<E, M, S> {
        let (owned, admitted) = &self.cells[at];
        let topology = self.graph.topology();

        let mut members = admitted.iter().copied().collect::<FxHashSet<_>>();
        let mut nodes = Vec::with_capacity(admitted.len());
        // Nodes riding along that the pass has yet to come to.
        let mut ahead = BinaryHeap::new();
        let mut admitted = admitted.iter().copied().peekable();

        loop {
            let riding = ahead.peek().map(|&Reverse(node)| node);
            let node = match (admitted.peek().copied(), riding) {
                (Some(next), Some(riding)) if riding < next => {
                    ahead.pop();
                    riding
                }
                (Some(next), _) => {
                    admitted.next();
                    next
                }
                (None, Some(riding)) => {
                    ahead.pop();
                    riding
                }
                (None, None) => break,
            };
            nodes.push((node, true));

            for &target in topology.targets(node) {
                if members.insert(target) {
                    if target > node {
                        ahead.push(Reverse(target));
                    } else {
                        nodes.push((target, false));
                    }
                }
            }
        }

        nodes.sort_unstable_by_key(|&(node, _)| node);
        let (nodes, leaving): (Vec<_>, Vec<_>) = nodes.into_iter().unzip();

        let mut loaded = FxHashSet::default();
        loaded.insert(*owned);

        ShardedNetwork::over(self.graph.carve(&nodes, &leaving), *owned, loaded)
    }
}

impl<E, M, S> ShardedNetwork<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    /// Every shard of a columnar `graph`. See [`Carving`].
    pub fn carve<'a, St>(
        graph: &'a Graph<E, M>,
        strategy: &St,
        padding_distance: f64,
    ) -> Carving<'a, E, M, S>
    where
        St: ShardingStrategy<Id = S>,
    {
        Carving::new(graph, strategy, padding_distance)
    }
}
