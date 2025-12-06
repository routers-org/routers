use crate::{DirectionAwareEdgeId, Edge, FatEdge, PredicateCache};
use routers_codec::primitive::{Entry, Metadata, Node};

use geo::Point;
use petgraph::prelude::DiGraphMap;
use rstar::RTree;
use rustc_hash::{FxHashMap, FxHasher};

use petgraph::Directed;
use std::fmt::{Debug, Formatter};
use std::hash::BuildHasherDefault;
use std::sync::{Arc, RwLock};
#[cfg(feature = "tracing")]
use tracing::Level;

pub type Weight = u32;

pub type LockedGraph<A, B> = Arc<RwLock<petgraph::Graph<A, B, Directed>>>;

pub type GraphStructure<E> =
    DiGraphMap<E, (Weight, DirectionAwareEdgeId<E>), BuildHasherDefault<FxHasher>>;

pub(crate) const MAX_WEIGHT: Weight = u32::MAX as Weight;

/// Routing graph.
///
/// TODO: ... can be ingested from an `.osm.pbf` file, and can be actioned upon using `route(start, end)`.
pub struct Graph<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub(crate) graph: GraphStructure<E>,
    pub(crate) hash: FxHashMap<E, Node<E>>,
    pub(crate) meta: FxHashMap<E, M>,

    pub(crate) index: RTree<Node<E>>,
    pub(crate) index_edge: RTree<FatEdge<E>>,

    pub cache: PredicateCache<E, M>,
}

impl<E, M> Debug for Graph<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Graph with Nodes: {}", self.hash.len())
    }
}

impl<E, M> Graph<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn index(&self) -> &RTree<Node<E>> {
        &self.index
    }

    pub fn index_edge(&self) -> &RTree<FatEdge<E>> {
        &self.index_edge
    }

    pub fn size(&self) -> usize {
        self.hash.len()
    }

    /// Safety: Assumes the edge exist
    pub fn meta(&self, edge: &DirectionAwareEdgeId<E>) -> &M {
        let index = edge.index();
        unsafe { self.meta.get(&index).unwrap_unchecked() }
    }

    #[inline]
    pub fn get_position(&self, node_index: &E) -> Option<Point<f64>> {
        self.hash.get(node_index).map(|point| point.position)
    }

    #[inline]
    pub fn get_line(&self, nodes: &[E]) -> Vec<Point<f64>> {
        nodes
            .iter()
            .filter_map(|node| self.get_position(node))
            .collect::<Vec<_>>()
    }

    /// Obtain the [edge](Edge), should it exist, between two [nodes](NodeIx) (specified as ids)
    pub fn edge(&self, a: &E, b: &E) -> Option<Edge<E>> {
        let edge = self.graph.edge_weight(*a, *b)?;
        Some(Edge::from((*a, *b, edge)))
    }
}
