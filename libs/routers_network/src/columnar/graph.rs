//! A [`Topology`] with its edge metadata and spatial indices: a whole network.

use alloc::sync::Arc;
use core::fmt::Debug;
use std::sync::{Mutex, PoisonError};

use geo::{Point, Rect};
use serde::de::DeserializeOwned;

use super::index::{Level, Packed};
use super::{EdgeIx, RawEdge, Topology, blob, unblob};
use crate::edge::Weight;
use crate::network::GraphEdge;
use crate::{DataPlane, Discovery, Edge, Entry, Metadata, Node, Route, Scan, envelope_of};

/// Which edges each runtime seen so far may traverse.
pub struct Masks<R>(Mutex<Vec<(R, Arc<[bool]>)>>);

impl<R> Default for Masks<R> {
    fn default() -> Self {
        Masks(Mutex::new(Vec::new()))
    }
}

impl<R: Clone + PartialEq> Masks<R> {
    /// The mask for `runtime`, built the first time it is asked for.
    pub fn get(&self, runtime: &R, build: impl FnOnce() -> Arc<[bool]>) -> Arc<[bool]> {
        let known = |masks: &[(R, Arc<[bool]>)]| {
            masks
                .iter()
                .find(|(seen, _)| seen == runtime)
                .map(|(_, mask)| Arc::clone(mask))
        };

        if let Some(mask) = known(&self.0.lock().unwrap_or_else(PoisonError::into_inner)) {
            return mask;
        }

        // Built with the lock released: a caller may be on a thread pool
        // that hands it other work, which must not find this lock held.
        let mask = build();

        let mut masks = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(raced) = known(&masks) {
            return raced;
        }
        masks.push((runtime.clone(), Arc::clone(&mask)));
        mask
    }
}

/// A routing network held as columns.
pub struct Graph<E: Entry, M: Metadata> {
    topology: Topology<E>,
    /// Per way of the topology: its metadata.
    meta: Vec<M>,

    node_index: Packed,
    edge_index: Packed,
    passable: Masks<M::Runtime>,
}

impl<E: Entry, M: Metadata> Default for Graph<E, M> {
    fn default() -> Self {
        Self::assemble(Vec::new(), Vec::new(), Vec::new())
    }
}

impl<E: Entry, M: Metadata> Debug for Graph<E, M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "Graph(nodes={}, edges={})",
            self.num_nodes(),
            self.num_edges()
        )
    }
}

impl<E: Entry, M: Metadata> Graph<E, M> {
    /// Lay a network out as columns, without spatial indices: see
    /// [`index`](Self::index).
    ///
    /// Of ways given twice, the first is kept. See [`Topology::assemble`]
    /// for how nodes and edges are treated.
    pub fn assemble(nodes: Vec<Node<E>>, edges: Vec<RawEdge<E>>, mut ways: Vec<(E, M)>) -> Self {
        ways.sort_by_key(|&(id, _)| id);
        ways.dedup_by_key(|&mut (id, _)| id);
        let (ways, meta) = ways.into_iter().unzip();

        Self::over(Topology::assemble(nodes, edges, ways), meta)
    }

    fn over(topology: Topology<E>, meta: Vec<M>) -> Self {
        Graph {
            topology,
            meta,
            node_index: Packed::default(),
            edge_index: Packed::default(),
            passable: Masks::default(),
        }
    }

    /// Build the spatial indices that [`Discovery`] and [`Scan`] answer from.
    ///
    /// Nodes and edges are already in spatial order, so this is one pass
    /// over each.
    pub fn index(&mut self) {
        let nodes = Level::around(self.topology.nodes().iter().map(|node| node.position));
        let edges = nodes.spanning(self.topology.spans());

        self.node_index = Packed::over(nodes);
        self.edge_index = Packed::over(edges);
    }

    fn envelope(&self, edge: usize) -> (Point, Point) {
        let (source, target) = self.topology.endpoints(edge as EdgeIx);
        envelope_of(source.position, target.position)
    }

    /// [`index`](Self::index), by value.
    pub fn indexed(mut self) -> Self {
        self.index();
        self
    }

    /// Rows in the node and edge spatial indices.
    pub fn index_sizes(&self) -> (usize, usize) {
        (self.node_index.len(), self.edge_index.len())
    }

    /// Append the network to `out`. The spatial indices are not encoded.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), String> {
        self.topology.encode(out)?;
        blob(out, &self.meta)
    }

    /// Read a network [encoded](Self::encode) at the head of `bytes`,
    /// without spatial indices.
    pub fn decode(bytes: &mut &[u8]) -> Result<Self, String>
    where
        E: DeserializeOwned,
        M: DeserializeOwned,
    {
        let topology = Topology::decode(bytes)?;
        let meta: Vec<M> = unblob(bytes)?;

        if meta.len() != topology.ways().len() {
            return Err("graph: way columns".to_string());
        }

        Ok(Self::over(topology, meta))
    }

    /// The part of this network over `nodes`, without spatial indices; see
    /// [`Topology::carve`].
    pub fn carve(&self, nodes: &[super::NodeIx], leaving: &[bool]) -> Self {
        let (topology, ways) = self.topology.carve(nodes, leaving);
        let meta = ways
            .iter()
            .map(|&way| self.meta[way as usize].clone())
            .collect();

        Self::over(topology, meta)
    }

    pub fn topology(&self) -> &Topology<E> {
        &self.topology
    }

    pub fn num_nodes(&self) -> usize {
        self.topology.num_nodes()
    }

    pub fn num_edges(&self) -> usize {
        self.topology.num_edges()
    }

    /// Every node of the network.
    pub fn nodes(&self) -> impl Iterator<Item = &Node<E>> {
        self.topology.nodes().iter()
    }

    /// Every directed edge of the network, grouped by the node it leaves.
    pub fn edges(&self) -> impl Iterator<Item = GraphEdge<E>> + '_ {
        self.topology.edges()
    }

    /// Every way of the network with its metadata.
    pub fn ways(&self) -> impl Iterator<Item = (E, &M)> {
        self.topology.ways().iter().copied().zip(&self.meta)
    }
}

impl<E: Entry, M: Metadata> DataPlane for Graph<E, M> {
    type Entry = E;
    type Runtime = M::Runtime;
    type Meta = M;

    fn metadata(&self, id: &E) -> Option<&M> {
        self.meta.get(self.topology.way_index(id)?)
    }

    fn point(&self, id: &E) -> Option<Point> {
        self.topology.node(id).map(|node| node.position)
    }

    fn edges_into<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(self.topology.edges_into(id))
    }

    fn edges_outof<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a> {
        Box::new(self.topology.edges_outof(id))
    }

    fn fatten(&self, edge: &Edge<E>) -> Option<Edge<Node<E>>> {
        self.topology.fatten(edge)
    }

    fn reach(&self, root: &E, reach: f64, runtime: &M::Runtime) -> Option<Vec<(E, E)>> {
        let passable = self.passable.get(runtime, || {
            self.topology
                .passable(|way, direction| self.meta[way].accessible(runtime, direction))
        });

        self.topology.reach(root, reach, &passable)
    }
}

impl<E: Entry, M: Metadata> Discovery for Graph<E, M> {
    fn edges_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = Edge<Node<E>>> + Send + 'a> {
        Box::new(
            self.edge_index
                .search(bounds, |edge| self.envelope(edge))
                .into_iter()
                .map(|edge| self.topology.fat_edge(edge)),
        )
    }

    fn nodes_in_box<'a>(
        &'a self,
        bounds: Rect<f64>,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a> {
        let nodes = self.topology.nodes();

        Box::new(
            self.node_index
                .search(bounds, |node| (nodes[node].position, nodes[node].position))
                .into_iter()
                .map(move |node| &nodes[node as usize]),
        )
    }

    fn node(&self, id: &E) -> Option<&Node<E>> {
        self.topology.node(id)
    }

    fn edge(&self, source: &E, target: &E) -> Option<Edge<E>> {
        self.topology.edge(source, target)
    }
}

impl<E: Entry, M: Metadata> Scan for Graph<E, M> {
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<E>> {
        let nodes = self.topology.nodes();

        self.node_index
            .nearest(point, |node| nodes[node].position)
            .map(|node| &nodes[node as usize])
    }
}

impl<E: Entry, M: Metadata> Route for Graph<E, M> {
    fn route_nodes(&self, start: E, finish: E) -> Option<(Weight, Vec<Node<E>>)> {
        self.topology.route_nodes(start, finish)
    }
}

/// Implements the network traits for a type by forwarding to a field that
/// already implements them, such as a [`Graph`].
///
/// ```ignore
/// columnar_network!(impl[] OsmNetwork => graph: Graph<OsmEntryId, OsmEdgeMetadata>);
/// ```
#[macro_export]
macro_rules! columnar_network {
    (impl[$($generics:tt)*] $ty:ty => $field:ident: $inner:ty) => {
        impl<$($generics)*> $crate::DataPlane for $ty {
            type Entry = <$inner as $crate::DataPlane>::Entry;
            type Runtime = <$inner as $crate::DataPlane>::Runtime;
            type Meta = <$inner as $crate::DataPlane>::Meta;

            fn metadata(&self, id: &Self::Entry) -> Option<&Self::Meta> {
                $crate::DataPlane::metadata(&self.$field, id)
            }

            fn point(&self, id: &Self::Entry) -> Option<$crate::columnar::__private::Point> {
                $crate::DataPlane::point(&self.$field, id)
            }

            fn edges_into<'a>(
                &'a self,
                id: Self::Entry,
            ) -> Box<dyn Iterator<Item = $crate::network::GraphEdge<Self::Entry>> + 'a> {
                $crate::DataPlane::edges_into(&self.$field, id)
            }

            fn edges_outof<'a>(
                &'a self,
                id: Self::Entry,
            ) -> Box<dyn Iterator<Item = $crate::network::GraphEdge<Self::Entry>> + 'a> {
                $crate::DataPlane::edges_outof(&self.$field, id)
            }

            fn fatten(
                &self,
                edge: &$crate::Edge<Self::Entry>,
            ) -> Option<$crate::Edge<$crate::Node<Self::Entry>>> {
                $crate::DataPlane::fatten(&self.$field, edge)
            }

            fn reach(
                &self,
                root: &Self::Entry,
                reach: f64,
                runtime: &Self::Runtime,
            ) -> Option<Vec<(Self::Entry, Self::Entry)>> {
                $crate::DataPlane::reach(&self.$field, root, reach, runtime)
            }
        }

        impl<$($generics)*> $crate::Discovery for $ty {
            fn edges_in_box<'a>(
                &'a self,
                bounds: $crate::columnar::__private::Rect<f64>,
            ) -> Box<dyn Iterator<Item = $crate::Edge<$crate::Node<Self::Entry>>> + Send + 'a> {
                $crate::Discovery::edges_in_box(&self.$field, bounds)
            }

            fn nodes_in_box<'a>(
                &'a self,
                bounds: $crate::columnar::__private::Rect<f64>,
            ) -> Box<dyn Iterator<Item = &'a $crate::Node<Self::Entry>> + Send + 'a> {
                $crate::Discovery::nodes_in_box(&self.$field, bounds)
            }

            fn node(&self, id: &Self::Entry) -> Option<&$crate::Node<Self::Entry>> {
                $crate::Discovery::node(&self.$field, id)
            }

            fn edge(
                &self,
                source: &Self::Entry,
                target: &Self::Entry,
            ) -> Option<$crate::Edge<Self::Entry>> {
                $crate::Discovery::edge(&self.$field, source, target)
            }
        }

        impl<$($generics)*> $crate::Scan for $ty {
            fn nearest_node<'a>(
                &'a self,
                point: &$crate::columnar::__private::Point,
            ) -> Option<&'a $crate::Node<Self::Entry>> {
                $crate::Scan::nearest_node(&self.$field, point)
            }
        }

        impl<$($generics)*> $crate::Route for $ty {
            fn route_nodes(
                &self,
                start: Self::Entry,
                finish: Self::Entry,
            ) -> Option<($crate::edge::Weight, Vec<$crate::Node<Self::Entry>>)> {
                $crate::Route::route_nodes(&self.$field, start, finish)
            }
        }
    };
}
