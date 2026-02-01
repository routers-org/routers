use crate::primitive::{DirectionAwareEdgeId, FatEdge, Node};
use crate::{Entry, Metadata};
use core::fmt::{Debug, Formatter};
use core::hash::BuildHasherDefault;
use geo::Point;
use petgraph::prelude::DiGraphMap;
use rstar::RTree;
use rustc_hash::{FxHashMap, FxHasher};

pub type Weight = u32;

pub type GraphStructure<E> =
    DiGraphMap<E, (Weight, DirectionAwareEdgeId<E>), BuildHasherDefault<FxHasher>>;

pub const MAX_WEIGHT: Weight = u32::MAX as Weight;

/// Routing graph.
pub struct Graph<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub graph: GraphStructure<E>,
    pub hash: FxHashMap<E, Node<E>>,
    pub meta: FxHashMap<E, M>,

    pub index: RTree<Node<E>>,
    pub index_edge: RTree<FatEdge<E>>,
}

impl<E, M> Debug for Graph<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
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
        self.meta.get(&index).unwrap()
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
}
