use crate::transition::WeightAndDistance;

use indexmap::IndexMap;
use indexmap::map::Entry;
use pathfinding::num_traits::Zero;
use rustc_hash::{FxHashSet, FxHasher};
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::hash::{BuildHasherDefault, Hash};

type FxIndexMap<K, V> = IndexMap<K, V, BuildHasherDefault<FxHasher>>;

type Cost = WeightAndDistance;

#[derive(Debug)]
struct SmallestHolder {
    cost: Cost,
    index: usize,
}

impl PartialEq for SmallestHolder {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.cost == other.cost
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
        other.cost.cmp(&self.cost)
    }
}

/// Struct returned by [`dijkstra_reach`].
pub struct DijkstraReachable<FN, E>
where
    E: routers_codec::Entry,
{
    to_see: BinaryHeap<SmallestHolder>,
    seen: FxHashSet<usize>,
    parents: FxIndexMap<E, (usize, Cost)>,
    successors: FN,
}

/// Information about a node reached by [`dijkstra_reach`].
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct DijkstraReachableItem<E>
where
    E: routers_codec::Entry,
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
    E: routers_codec::Entry,
{
    type Item = DijkstraReachableItem<E>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(SmallestHolder { cost, index }) = self.to_see.pop() {
            if !self.seen.insert(index) {
                continue;
            }

            let (item, successors) = {
                let (node, (parent_index, cost)) = self.parents.get_index(index).unwrap();
                let item = Some(DijkstraReachableItem {
                    node: *node,
                    parent: self.parents.get_index(*parent_index).map(|x| *x.0),
                    total_cost: *cost,
                });

                (item, (self.successors)(node))
            };

            for (successor, move_cost) in successors {
                let new_cost = cost + move_cost;

                let index = match self.parents.entry(successor) {
                    Entry::Vacant(e) => {
                        let n = e.index();
                        e.insert((index, new_cost));
                        n
                    }
                    Entry::Occupied(mut e) => {
                        if e.get().1 > new_cost {
                            e.insert((index, new_cost));
                            e.index()
                        } else {
                            continue;
                        }
                    }
                };

                self.to_see.push(SmallestHolder {
                    cost: new_cost,
                    index,
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
        E: routers_codec::Entry,
        FN: FnMut(&E) -> IN,
        IN: Iterator<Item = (E, Cost)>,
    {
        let mut to_see: BinaryHeap<SmallestHolder> = BinaryHeap::with_capacity(256);
        to_see.push(SmallestHolder {
            cost: Zero::zero(),
            index: 0,
        });

        let mut parents: FxIndexMap<E, (usize, Cost)> =
            FxIndexMap::with_capacity_and_hasher(64, BuildHasherDefault::<FxHasher>::default());

        parents.insert(*start, (usize::MAX, Zero::zero()));
        let seen = FxHashSet::default();

        DijkstraReachable {
            to_see,
            seen,
            parents,
            successors,
        }
    }
}
