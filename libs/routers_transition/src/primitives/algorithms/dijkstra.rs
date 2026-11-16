use alloc::collections::BinaryHeap;
use core::cmp::Ordering;
use core::hash::{BuildHasherDefault, Hash};
use indexmap::IndexMap;
use indexmap::map::Entry;
use num_traits::Zero;
use rustc_hash::FxHasher;

use crate::primitives::WeightAndDistance;

type FxIndexMap<K, V> = IndexMap<K, V, BuildHasherDefault<FxHasher>>;

type Cost = WeightAndDistance;

/// A frontier entry, ordered by `key`: the cost's [`repr`](Cost::repr).
#[derive(Debug)]
struct SmallestHolder {
    key: u32,
    cost: Cost,
    index: u32,
}

impl PartialEq for SmallestHolder {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for SmallestHolder {}

impl PartialOrd for SmallestHolder {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SmallestHolder {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

/// A discovered node's best parent, by index, and the cost of that route.
#[derive(Debug, Clone, Copy)]
struct Reached {
    parent: u32,
    cost: Cost,
    key: u32,
}

/// The root has no parent.
const NO_PARENT: u32 = u32::MAX;

/// Struct returned by [`dijkstra_reach`].
pub struct DijkstraReachable<FN, E>
where
    E: routers_network::Entry,
{
    to_see: BinaryHeap<SmallestHolder>,
    /// Settled nodes, by index into `parents`.
    seen: Vec<bool>,
    parents: FxIndexMap<E, Reached>,
    successors: FN,
}

/// Information about a node reached by [`dijkstra_reach`].
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct DijkstraReachableItem<E>
where
    E: routers_network::Entry,
{
    /// The node that was reached by [`dijkstra_reach`].
    pub node: E,
    /// The previous node that the current node came from.
    /// If the node is the first node, there will be no parent.
    pub parent: Option<E>,
    /// The total cost from the starting node.
    pub total_cost: Cost,
}

impl<FN, IN, E> Iterator for DijkstraReachable<FN, E>
where
    FN: FnMut(&E) -> IN,
    IN: Iterator<Item = (E, Cost)>,
    E: routers_network::Entry,
{
    type Item = DijkstraReachableItem<E>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(SmallestHolder { cost, index, .. }) = self.to_see.pop() {
            if core::mem::replace(&mut self.seen[index as usize], true) {
                continue;
            }

            let (item, successors) = {
                let (node, reached) = self.parents.get_index(index as usize).unwrap();
                let item = Some(DijkstraReachableItem {
                    node: *node,
                    parent: self
                        .parents
                        .get_index(reached.parent as usize)
                        .map(|x| *x.0),
                    total_cost: reached.cost,
                });

                (item, (self.successors)(node))
            };

            for (successor, move_cost) in successors {
                let new_cost = cost + move_cost;
                let key = new_cost.repr();
                let reached = Reached {
                    parent: index,
                    cost: new_cost,
                    key,
                };

                let index = match self.parents.entry(successor) {
                    Entry::Vacant(e) => {
                        let n = e.index();
                        e.insert(reached);
                        self.seen.push(false);
                        n
                    }
                    Entry::Occupied(mut e) => {
                        if e.get().key > key {
                            e.insert(reached);
                            e.index()
                        } else {
                            continue;
                        }
                    }
                };

                self.to_see.push(SmallestHolder {
                    key,
                    cost: new_cost,
                    index: index as u32,
                });
            }

            return item;
        }

        None
    }
}

pub struct Dijkstra;

impl Dijkstra {
    /// Visit all nodes that are reachable from a start node. The node
    /// will be visited in order of cost, with the closest nodes first.
    ///
    /// The `successors` function receives the current node, and returns
    /// an iterator of successors associated with their move cost.
    pub fn reach<FN, IN, E>(&self, start: &E, successors: FN) -> DijkstraReachable<FN, E>
    where
        E: routers_network::Entry,
        FN: FnMut(&E) -> IN,
        IN: Iterator<Item = (E, Cost)>,
    {
        let origin: Cost = Zero::zero();

        let mut to_see: BinaryHeap<SmallestHolder> = BinaryHeap::with_capacity(256);
        to_see.push(SmallestHolder {
            key: origin.repr(),
            cost: origin,
            index: 0,
        });

        let mut parents: FxIndexMap<E, Reached> =
            FxIndexMap::with_capacity_and_hasher(64, BuildHasherDefault::<FxHasher>::default());

        parents.insert(
            *start,
            Reached {
                parent: NO_PARENT,
                cost: origin,
                key: origin.repr(),
            },
        );

        let mut seen = Vec::with_capacity(64);
        seen.push(false);

        DijkstraReachable {
            to_see,
            seen,
            parents,
            successors,
        }
    }
}
