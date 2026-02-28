use geo::Point;

use crate::{Entry, Node, Scan, edge::Weight};

pub trait Route<E>: Scan<E>
where
    E: Entry,
{
    // Note to self, allow changing the strategy by making this a provider not just an attachment trait
    /// TODO: Routes ...
    fn route_nodes(&self, start_node: E, finish_node: E) -> Option<(Weight, Vec<Node<E>>)>;

    /// Finds the optimal route between a start and end point.
    /// Returns the weight and routing node vector.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = Level::INFO))]
    fn route_points(&self, start: &Point, finish: &Point) -> Option<(Weight, Vec<Node<E>>)> {
        let start_node = self.nearest_node(start)?;
        let finish_node = self.nearest_node(finish)?;

        self.route_nodes(start_node.id, finish_node.id)
    }
}
