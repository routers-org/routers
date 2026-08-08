use alloc::collections::BinaryHeap;
use core::cmp::Ordering;
use core::hash::{BuildHasherDefault, Hash};
use core::mem;
use indexmap::IndexMap;
use indexmap::map::Entry;
use pathfinding::num_traits::Zero;
use rustc_hash::{FxHashSet, FxHasher};
use std::any::Any;
use std::cell::RefCell;

use crate::primitives::WeightAndDistance;

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

/// The working buffers of one bounded reachability search — the open set, the
/// visited set, and the parent/cost table.
///
/// A full 2 km Dijkstra grows these to thousands of entries, and the predicate
/// cache reruns one per miss. This keeps one set per thread in [`SCRATCH`] and
/// reuses it, retaining capacity across searches — the buffers never escape a
/// single `reach`, so there is no aliasing. Reuse is a pure allocation
/// optimisation: the search logic in [`DijkstraReachable::next`] is unchanged,
/// so results are identical.
struct Scratch<E>
where
    E: routers_network::Entry,
{
    to_see: BinaryHeap<SmallestHolder>,
    seen: FxHashSet<usize>,
    parents: FxIndexMap<E, (usize, Cost)>,
}

// Manual (not derived) so we don't require `E: Default`.
impl<E> Default for Scratch<E>
where
    E: routers_network::Entry,
{
    fn default() -> Self {
        Self {
            to_see: BinaryHeap::new(),
            seen: FxHashSet::default(),
            parents: FxIndexMap::default(),
        }
    }
}

thread_local! {
    // One reusable scratch per thread, type-erased so a single `thread_local`
    // serves any `Entry` type. In practice a process routes over one network
    // (one `Entry`), so the downcast hits; a miss simply allocates fresh.
    static SCRATCH: RefCell<Option<Box<dyn Any>>> = const { RefCell::new(None) };
}

impl<E> Scratch<E>
where
    E: routers_network::Entry + 'static,
{
    /// Take this thread's scratch (cleared, capacity retained), or a fresh one.
    fn take() -> Self {
        SCRATCH.with(|slot| {
            slot.borrow_mut()
                .take()
                .and_then(|any| any.downcast::<Scratch<E>>().ok())
                .map(|mut boxed| {
                    boxed.to_see.clear();
                    boxed.seen.clear();
                    boxed.parents.clear();
                    *boxed
                })
                .unwrap_or_default()
        })
    }

    /// Return this scratch to the thread pool for the next search to reuse.
    fn give(self) {
        SCRATCH.with(|slot| *slot.borrow_mut() = Some(Box::new(self) as Box<dyn Any>));
    }
}

/// Struct returned by [`dijkstra_reach`].
pub struct DijkstraReachable<FN, E>
where
    E: routers_network::Entry + 'static,
{
    scratch: Scratch<E>,
    successors: FN,
}

impl<FN, E> Drop for DijkstraReachable<FN, E>
where
    E: routers_network::Entry + 'static,
{
    fn drop(&mut self) {
        // Hand the buffers back to the thread pool (they are cleared on reuse).
        mem::take(&mut self.scratch).give();
    }
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
    E: routers_network::Entry + 'static,
{
    type Item = DijkstraReachableItem<E>;

    fn next(&mut self) -> Option<Self::Item> {
        let Scratch {
            to_see,
            seen,
            parents,
        } = &mut self.scratch;

        while let Some(SmallestHolder { cost, index }) = to_see.pop() {
            if !seen.insert(index) {
                continue;
            }

            let (item, successors) = {
                let (node, (parent_index, cost)) = parents.get_index(index).unwrap();
                let item = Some(DijkstraReachableItem {
                    node: *node,
                    parent: parents.get_index(*parent_index).map(|x| *x.0),
                    total_cost: *cost,
                });

                (item, (self.successors)(node))
            };

            for (successor, move_cost) in successors {
                let new_cost = cost + move_cost;

                let index = match parents.entry(successor) {
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

                to_see.push(SmallestHolder {
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
        E: routers_network::Entry + 'static,
        FN: FnMut(&E) -> IN,
        IN: Iterator<Item = (E, Cost)>,
    {
        let mut scratch = Scratch::<E>::take();
        scratch.to_see.push(SmallestHolder {
            cost: Zero::zero(),
            index: 0,
        });
        scratch.parents.insert(*start, (usize::MAX, Zero::zero()));

        DijkstraReachable { scratch, successors }
    }
}
