//! Searches over a graph's columns.

use alloc::collections::BinaryHeap;
use core::cmp::Ordering;

use uom::si::f64::Length;
use uom::si::length::centimeter;

use super::{EdgeIx, NodeIx};
use crate::edge::Weight;

/// "No such node", where a [`NodeIx`] is optional.
pub(super) const NO_NODE: NodeIx = NodeIx::MAX;

/// What a search reads of a graph.
#[derive(Debug, Default)]
pub(super) struct Csr {
    /// Node `n`'s out-edges are `offsets[n]..offsets[n + 1]`.
    pub(super) offsets: Vec<u32>,
    pub(super) targets: Vec<NodeIx>,
    /// Per edge: its road-class weight.
    pub(super) class: Vec<Weight>,
    /// Per edge: its Haversine length in whole centimetres.
    pub(super) length_cm: Vec<u32>,
}

impl Csr {
    #[inline]
    pub(super) fn out_edges(&self, node: NodeIx) -> core::ops::Range<EdgeIx> {
        self.offsets[node as usize]..self.offsets[node as usize + 1]
    }
}

/// A path cost as the matcher accumulates it.
#[derive(Clone, Copy, Debug, Default)]
struct Cost {
    class_sum: u32,
    edges: u32,
    distance_cm: u32,
}

impl Cost {
    #[inline]
    fn then(self, class: u32, length_cm: u32) -> Self {
        Cost {
            class_sum: self.class_sum + class,
            edges: self.edges + 1,
            distance_cm: self.distance_cm + length_cm,
        }
    }

    /// The ordering key: mean road class, squared, times distance.
    ///
    /// The matcher takes this product in `f64` and casts it to `u32`. Every
    /// product that fits a `u32` is exact in an `f64`, and the cast saturates
    /// as this does, so the two agree to the bit.
    #[inline]
    fn key(self) -> u32 {
        let class = u64::from(self.class_sum.checked_div(self.edges).unwrap_or(0));
        let key = (class * class).saturating_mul(u64::from(self.distance_cm));

        key.min(u64::from(u32::MAX)) as u32
    }
}

#[derive(Clone, Copy, Debug)]
struct Label {
    node: NodeIx,
    /// The label this one was reached from; itself, for the root.
    parent: u32,
    cost: Cost,
    key: u32,
}

/// A frontier entry, ordered by key alone: smallest first.
///
/// It names its label rather than carrying the label's cost. The first entry
/// popped for a label is the one last pushed for it, since a label is only
/// pushed again at a smaller key; so the label still holds that entry's cost.
#[derive(Debug)]
struct Open {
    key: u32,
    label: u32,
}

impl PartialEq for Open {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key
    }
}

impl Eq for Open {}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other.key.cmp(&self.key)
    }
}

/// The least distance, in centimetres, that is not within `reach`.
///
/// "Within" is the matcher's own comparison, of lengths. It holds for every
/// distance below some point and none from it on, so that point is found
/// once here rather than the comparison made at every node. A reach that
/// compares with nothing (NaN) has nothing within it.
fn bound(reach: Length) -> u64 {
    let within = |distance_cm: u32| Length::new::<centimeter>(f64::from(distance_cm)) < reach;

    if within(u32::MAX) {
        return u64::from(u32::MAX) + 1;
    }

    let (mut low, mut high) = (0u32, u32::MAX);
    while low < high {
        let middle = low + (high - low) / 2;
        if within(middle) {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    u64::from(low)
}

/// A reusable search workspace.
#[derive(Debug, Default)]
pub(super) struct Search {
    /// Generation each node was last discovered in, and its label then.
    stamp: Vec<u32>,
    slot: Vec<u32>,
    generation: u32,

    labels: Vec<Label>,
    seen: Vec<bool>,
    open: BinaryHeap<Open>,
    settled: Vec<(NodeIx, NodeIx)>,
}

impl Search {
    pub(super) fn new(nodes: usize) -> Self {
        Search {
            stamp: vec![0; nodes],
            slot: vec![0; nodes],
            ..Search::default()
        }
    }

    /// The matcher's bounded search: every node settled before the first one
    /// at `reach` or beyond, as `(node, parent)` in the order settled. The
    /// root's parent is [`NO_NODE`].
    pub(super) fn reach(
        &mut self,
        graph: &Csr,
        passable: &[bool],
        root: NodeIx,
        reach: Length,
    ) -> &[(NodeIx, NodeIx)] {
        self.begin();
        self.settled.clear();
        self.discover(root, 0, Cost::default());

        let bound = bound(reach);

        while let Some(Open { label, .. }) = self.open.pop() {
            if core::mem::replace(&mut self.seen[label as usize], true) {
                continue;
            }

            let Label {
                node, parent, cost, ..
            } = self.labels[label as usize];

            if u64::from(cost.distance_cm) >= bound {
                break;
            }

            let from = if parent == label {
                NO_NODE
            } else {
                self.labels[parent as usize].node
            };
            self.settled.push((node, from));

            for edge in graph.out_edges(node) {
                let edge = edge as usize;
                if !passable[edge] {
                    continue;
                }

                let next = cost.then(graph.class[edge], graph.length_cm[edge]);
                self.relax(graph.targets[edge], label, next);
            }
        }

        &self.settled
    }

    /// The least-cost route from `from` to `to` by summed road class, as its
    /// cost and its nodes. Stops once `to` is settled.
    pub(super) fn route(
        &mut self,
        graph: &Csr,
        from: NodeIx,
        to: NodeIx,
    ) -> Option<(u32, Vec<NodeIx>)> {
        let cost = |total: u32| Cost {
            class_sum: total,
            ..Cost::default()
        };

        self.begin();
        self.discover(from, 0, cost(0));

        while let Some(Open { key, label, .. }) = self.open.pop() {
            if core::mem::replace(&mut self.seen[label as usize], true) {
                continue;
            }

            let node = self.labels[label as usize].node;
            if node == to {
                let mut path = vec![node];
                let mut cursor = label;
                while self.labels[cursor as usize].parent != cursor {
                    cursor = self.labels[cursor as usize].parent;
                    path.push(self.labels[cursor as usize].node);
                }
                path.reverse();
                return Some((key, path));
            }

            for edge in graph.out_edges(node) {
                let edge = edge as usize;
                let (next, total) = (graph.targets[edge], key + graph.class[edge]);

                if self.stamp[next as usize] != self.generation {
                    self.place(next, label, cost(total), total);
                } else {
                    let known = &mut self.labels[self.slot[next as usize] as usize];
                    if known.key <= total {
                        continue;
                    }
                    known.parent = label;
                    known.key = total;
                }

                self.open.push(Open {
                    key: total,
                    label: self.slot[next as usize],
                });
            }
        }

        None
    }

    /// Start a new search: nothing discovered before counts.
    fn begin(&mut self) {
        self.labels.clear();
        self.seen.clear();
        self.open.clear();

        self.generation = self.generation.wrapping_add(1);
        if self.generation == 0 {
            // Back at a generation an old stamp may still carry.
            self.stamp.fill(0);
            self.generation = 1;
        }
    }

    /// Label `node`, without queueing it.
    fn place(&mut self, node: NodeIx, parent: u32, cost: Cost, key: u32) -> u32 {
        let label = self.labels.len() as u32;
        let parent = if self.labels.is_empty() {
            label
        } else {
            parent
        };

        self.stamp[node as usize] = self.generation;
        self.slot[node as usize] = label;
        self.labels.push(Label {
            node,
            parent,
            cost,
            key,
        });
        self.seen.push(false);
        label
    }

    /// First sight of `node`.
    fn discover(&mut self, node: NodeIx, parent: u32, cost: Cost) {
        let key = cost.key();
        let label = self.place(node, parent, cost, key);

        self.open.push(Open { key, label });
    }

    /// Offer `node` a route through `parent` at `cost`.
    #[inline]
    fn relax(&mut self, node: NodeIx, parent: u32, cost: Cost) {
        if self.stamp[node as usize] != self.generation {
            return self.discover(node, parent, cost);
        }

        let label = self.slot[node as usize];
        let known = &mut self.labels[label as usize];
        let key = cost.key();

        if known.key > key {
            known.parent = parent;
            known.cost = cost;
            known.key = key;
            self.open.push(Open { key, label });
        }
    }
}
