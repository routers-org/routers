//! Searches over an [`OsmNetwork`](super::OsmNetwork)'s columns.

use alloc::collections::BinaryHeap;
use core::cmp::Ordering;

use uom::si::f64::Length;
use uom::si::length::centimeter;

use super::graph::{Columns, NodeIx};

/// "No such node", where a [`NodeIx`] is optional.
pub(super) const NO_NODE: NodeIx = NodeIx::MAX;

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
    #[inline]
    fn key(self) -> u32 {
        let class = self.class_sum.checked_div(self.edges).unwrap_or(0);

        (f64::from(class).powi(2) * f64::from(self.distance_cm)) as u32
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
#[derive(Debug)]
struct Open {
    key: u32,
    cost: Cost,
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
        graph: &Columns,
        passable: &[bool],
        root: NodeIx,
        reach: Length,
    ) -> &[(NodeIx, NodeIx)] {
        self.begin();
        self.settled.clear();
        self.discover(root, 0, Cost::default());

        while let Some(Open { cost, label, .. }) = self.open.pop() {
            if core::mem::replace(&mut self.seen[label as usize], true) {
                continue;
            }

            let Label {
                node,
                parent,
                cost: best,
                ..
            } = self.labels[label as usize];

            // Written as the matcher writes it, so a reach that compares
            // with nothing (NaN) ends the search here too.
            let within = Length::new::<centimeter>(f64::from(best.distance_cm)) < reach;
            if !within {
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
        graph: &Columns,
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
                    cost: cost(total),
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

        self.open.push(Open { key, cost, label });
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
            self.open.push(Open { key, cost, label });
        }
    }
}
