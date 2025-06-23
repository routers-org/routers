use crate::graph::Weight;

use geo::Point;
use routers_codec::{Entry, Node};

pub trait Route<E>
where
    E: Entry,
{
    /// TODO: Routes ...
    fn route_nodes(&self, start_node: E, finish_node: E) -> Option<(Weight, Vec<Node<E>>)>;

    /// Finds the optimal route between a start and end point.
    /// Returns the weight and routing node vector.
    fn route_points(&self, start: Point, finish: Point) -> Option<(Weight, Vec<Node<E>>)>;
}
