use core::fmt::Debug;

use crate::{DirectionAwareEdgeId, Edge, Entry, Metadata, Node, Route, Scan, edge::Weight};
use geo::Point;

pub type EdgeData<E> = (Weight, DirectionAwareEdgeId<E>);
pub type GraphEdge<E> = (E, E, EdgeData<E>);

pub trait Network<E, M>: Scan<E> + Route<E> + Debug + Send + Sync
where
    E: Entry,
    M: Metadata,
{
    fn metadata(&self, id: &E) -> Option<&M>;

    fn point(&self, id: &E) -> Option<Point>;

    fn edges_outof<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a>;
    fn edges_into<'a>(&'a self, id: E) -> Box<dyn Iterator<Item = GraphEdge<E>> + 'a>;

    /// Produces an iterator of points for a given input.
    ///
    /// All provided nodes that do not exist will not be returned, so the iterator's
    /// length may be smaller than the input slice.
    fn line(&self, nodes: &[E]) -> Vec<Point> {
        nodes.iter().filter_map(|node| self.point(node)).collect()
    }

    fn fatten(&self, edge: &Edge<E>) -> Option<Edge<Node<E>>>;
}
