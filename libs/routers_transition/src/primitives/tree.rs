//! The walked part of a bounded-Dijkstra tree, with its road geometry,
//! measured the first time a hop crosses each segment.

use routers_network::{Entry, Network, Node};
use rustc_hash::FxHashMap;

use crate::map_path::{Segment, Stride};

/// One node of a [`Tree`].
#[derive(Debug)]
pub(crate) struct Link<E: Entry> {
    pub(crate) node: Node<E>,
    /// The road segment from the parent, and the turn taken onto it.
    pub(crate) stride: Stride,
    /// The most recent heading on the way here from the root, if any.
    heading: Option<f64>,
    /// Metres of road from the root, summed in path order.
    pub(crate) length: f64,
}

/// Where a node sits in a [`Tree`].
#[derive(Clone, Copy, Debug)]
pub(crate) enum Leaf {
    /// Reachable: the link it resolved to.
    At(u32),
    /// Outside the predicate bound.
    Unreachable,
    /// Reachable, but through a node the map has no position for.
    Unplaced,
}

/// The part of a [`Tree`] that the paths to some set of leaves cross: each
/// link once, and never before its parent.
#[derive(Debug, Default)]
pub(crate) struct Span {
    steps: Vec<Step>,
    /// Where each link of the tree sits in `steps`, if it does.
    slots: Vec<u32>,
    /// Scratch: links climbed through on the way up to one already placed.
    pending: Vec<u32>,
}

/// One link of a [`Span`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct Step {
    pub(crate) link: u32,
    /// The step of this link's parent: always an earlier one.
    pub(crate) parent: u32,
}

impl Span {
    const ABSENT: u32 = u32::MAX;

    /// The steps below the root, which is step `0` and is not among them.
    #[inline]
    pub(crate) fn descent(&self) -> &[Step] {
        self.steps.get(1..).unwrap_or_default()
    }

    /// The step at which `link` sits, for a link the span was made to reach.
    #[inline]
    pub(crate) fn step(&self, link: u32) -> Option<usize> {
        match self.slots.get(link as usize) {
            Some(&slot) if slot != Self::ABSENT => Some(slot as usize),
            _ => None,
        }
    }
}

/// The part of one root's bounded-Dijkstra tree that hops have walked so far.
#[derive(Debug)]
pub(crate) struct Tree<E: Entry> {
    links: Vec<Link<E>>,
    /// Per link, its parent and depth. Held apart from `links` so that walking
    /// a path to the root stays in cache.
    parents: Vec<u32>,
    depths: Vec<u32>,
    index: FxHashMap<E, u32>,
    /// Scratch: nodes walked on the way up to an already-resolved link.
    pending: Vec<E>,
}

impl<E: Entry> Tree<E> {
    const ROOT: u32 = 0;
    const UNREACHABLE: u32 = u32::MAX;
    const UNPLACED: u32 = u32::MAX - 1;

    pub(crate) fn new(root: Node<E>) -> Self {
        let mut index = FxHashMap::default();
        index.insert(root.id, Self::ROOT);

        Self {
            parents: vec![Self::ROOT],
            depths: vec![0],
            links: vec![Link {
                node: root,
                stride: Stride::after(None, Segment::between(root.position, root.position)),
                heading: None,
                length: 0.0,
            }],
            index,
            pending: Vec::new(),
        }
    }

    /// Where `node` sits, if it has been [resolved](Self::resolve) before.
    #[inline]
    pub(crate) fn find(&self, node: &E) -> Option<Leaf> {
        self.index.get(node).map(|&link| match link {
            Self::UNREACHABLE => Leaf::Unreachable,
            Self::UNPLACED => Leaf::Unplaced,
            link => Leaf::At(link),
        })
    }

    /// Resolve `leaf`, extending the tree with the unknown part of its path to
    /// the root. `parents` is the root's parent-pointer map.
    pub(crate) fn resolve<N>(&mut self, leaf: E, parents: &FxHashMap<E, E>, map: &N) -> Leaf
    where
        N: Network<Entry = E> + ?Sized,
    {
        self.pending.clear();

        // Climb until meeting a node already in the tree.
        let mut cursor = leaf;
        let mut base = loop {
            match self.find(&cursor) {
                Some(Leaf::At(link)) => break link,
                Some(dead) => return self.abandon(dead),
                None => {}
            }

            self.pending.push(cursor);
            match parents.get(&cursor) {
                // A parent map is a tree, so a climb longer than the map
                // itself cannot be heading for the root.
                Some(parent) if self.pending.len() <= parents.len() => cursor = *parent,
                _ => return self.abandon(Leaf::Unreachable),
            }
        };

        // Then descend again, measuring each new segment.
        while let Some(id) = self.pending.pop() {
            let Some(position) = map.point(&id) else {
                self.pending.push(id);
                return self.abandon(Leaf::Unplaced);
            };

            let parent = &self.links[base as usize];
            let segment = Segment::between(parent.node.position, position);

            let link = self.links.len() as u32;
            self.parents.push(base);
            self.depths.push(self.depths[base as usize] + 1);
            self.links.push(Link {
                node: Node::new(position, id),
                stride: Stride::after(parent.heading, segment),
                heading: segment.heading().or(parent.heading),
                length: parent.length + segment.distance,
            });
            self.index.insert(id, link);
            base = link;
        }

        Leaf::At(base)
    }

    /// Mark everything still pending as `dead`.
    fn abandon(&mut self, dead: Leaf) -> Leaf {
        let mark = match dead {
            Leaf::Unplaced => Self::UNPLACED,
            _ => Self::UNREACHABLE,
        };

        for id in self.pending.drain(..) {
            self.index.insert(id, mark);
        }

        dead
    }

    /// The links from the root down to `leaf`, inclusive.
    pub(crate) fn path(&self, leaf: u32, into: &mut Vec<u32>) {
        into.clear();
        into.resize(self.depths[leaf as usize] as usize + 1, Self::ROOT);

        let mut cursor = leaf;
        for link in into.iter_mut().rev() {
            *link = cursor;
            cursor = self.parents[cursor as usize];
        }
    }

    /// Lay out in `span` the links that the paths to `leaves` cross.
    pub(crate) fn span(&self, leaves: impl IntoIterator<Item = u32>, span: &mut Span) {
        span.steps.clear();
        span.steps.reserve(self.links.len());
        span.steps.push(Step {
            link: Self::ROOT,
            parent: 0,
        });

        span.slots.clear();
        span.slots.resize(self.links.len(), Span::ABSENT);
        span.slots[Self::ROOT as usize] = 0;

        for leaf in leaves {
            // Climb to a link already placed, then place the climb beneath it.
            let mut cursor = leaf;
            while span.slots[cursor as usize] == Span::ABSENT {
                span.pending.push(cursor);
                cursor = self.parents[cursor as usize];
            }

            let mut parent = span.slots[cursor as usize];
            span.steps.reserve(span.pending.len());
            while let Some(link) = span.pending.pop() {
                let slot = span.steps.len() as u32;
                span.steps.push(Step { link, parent });
                span.slots[link as usize] = slot;
                parent = slot;
            }
        }
    }

    /// Whether `link` is the root: the end of a path that crosses no edge.
    #[inline]
    pub(crate) fn is_root(&self, link: u32) -> bool {
        link == Self::ROOT
    }

    #[inline]
    pub(crate) fn link(&self, link: u32) -> &Link<E> {
        &self.links[link as usize]
    }
}
