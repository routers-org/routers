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
    f_cost: Cost,  // f(n) = g(n) + h(n) - used for priority queue ordering
    g_cost: Cost,  // g(n) = actual cost from start
    index: usize,
}

impl PartialEq for SmallestHolder {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.f_cost == other.f_cost
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
        other.f_cost.cmp(&self.f_cost)
    }
}

/// Struct returned by [`astar_reach`].
pub struct AStarReachable<FN, HN, E>
where
    E: routers_codec::Entry,
{
    to_see: BinaryHeap<SmallestHolder>,
    seen: FxHashSet<usize>,
    parents: FxIndexMap<E, (usize, Cost)>,
    successors: FN,
    heuristic: HN,
}

/// Information about a node reached by [`astar_reach`].
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct AStarReachableItem<E>
where
    E: routers_codec::Entry,
{
    /// The node that was reached by [`astar_reach`].
    pub node: E,
    /// The previous node that the current node came from.
    /// If the node is the first node, there will be no parent.
    pub parent: Option<E>,
    /// The total cost from the starting node.
    pub total_cost: Cost,
}

impl<FN, HN, IN, E> Iterator for AStarReachable<FN, HN, E>
where
    FN: FnMut(&E) -> IN,
    HN: FnMut(&E) -> Cost,
    IN: Iterator<Item = (E, Cost)>,
    E: routers_codec::Entry,
{
    type Item = AStarReachableItem<E>;

    fn next(&mut self) -> Option<Self::Item> {
        while let Some(SmallestHolder { f_cost: _, g_cost, index }) = self.to_see.pop() {
            if !self.seen.insert(index) {
                continue;
            }

            let (item, successors) = {
                let (node, (parent_index, stored_cost)) = self.parents.get_index(index).unwrap();

                // Skip if we've found a better path to this node since it was added to the queue
                if *stored_cost < g_cost {
                    continue;
                }

                let item = Some(AStarReachableItem {
                    node: *node,
                    parent: self.parents.get_index(*parent_index).map(|x| *x.0),
                    total_cost: *stored_cost,
                });

                (item, (self.successors)(node))
            };

            for (successor, move_cost) in successors {
                let new_g_cost = g_cost + move_cost;
                let h_cost = (self.heuristic)(&successor);
                let new_f_cost = new_g_cost + h_cost;

                let index = match self.parents.entry(successor) {
                    Entry::Vacant(e) => {
                        let n = e.index();
                        e.insert((index, new_g_cost));
                        n
                    }
                    Entry::Occupied(mut e) => {
                        if e.get().1 > new_g_cost {
                            e.insert((index, new_g_cost));
                            e.index()
                        } else {
                            continue;
                        }
                    }
                };

                self.to_see.push(SmallestHolder {
                    f_cost: new_f_cost,
                    g_cost: new_g_cost,
                    index,
                });
            }

            return item;
        }

        None
    }
}

pub struct AStar;

impl AStar {
    /// Visit all nodes that are reachable from a start node. The node
    /// will be visited in order of estimated total cost (f = g + h), with the
    /// most promising nodes first according to the A* algorithm.
    ///
    /// The `successors` function receives the current node, and returns
    /// an iterator of successors associated with their move cost.
    ///
    /// The `heuristic` function receives a node and returns an estimate
    /// of the cost from that node to the goal. For the algorithm to be
    /// optimal, this heuristic must be admissible (never overestimate).
    pub fn reach<FN, HN, IN, E>(&self, start: &E, successors: FN, heuristic: HN) -> AStarReachable<FN, HN, E>
    where
        E: routers_codec::Entry,
        FN: FnMut(&E) -> IN,
        HN: FnMut(&E) -> Cost,
        IN: Iterator<Item = (E, Cost)>,
    {
        let mut to_see: BinaryHeap<SmallestHolder> = BinaryHeap::with_capacity(256);

        // For the start node, f_cost = g_cost + h_cost = 0 + h(start)
        let start_h_cost = heuristic(start);
        to_see.push(SmallestHolder {
            f_cost: start_h_cost,
            g_cost: Zero::zero(),
            index: 0,
        });

        let mut parents: FxIndexMap<E, (usize, Cost)> =
            FxIndexMap::with_capacity_and_hasher(64, BuildHasherDefault::<FxHasher>::default());

        parents.insert(*start, (usize::MAX, Zero::zero()));
        let seen = FxHashSet::default();

        AStarReachable {
            to_see,
            seen,
            parents,
            successors,
            heuristic,
        }
    }
}