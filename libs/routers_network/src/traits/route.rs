use alloc::sync::Arc;

use geo::Point;

use crate::{Node, Scan, edge::Weight};
#[cfg(feature = "tracing")]
use tracing::Level;

pub trait Route: Scan {
    // Note to self, allow changing the strategy by making this a provider not just an attachment trait
    /// TODO: Routes ...
    fn route_nodes(
        &self,
        start_node: Self::Entry,
        finish_node: Self::Entry,
    ) -> Option<(Weight, Vec<Node<Self::Entry>>)>;

    /// Finds the optimal route between a start and end point.
    /// Returns the weight and routing node vector.
    #[cfg_attr(feature = "tracing", tracing::instrument(skip_all, level = Level::INFO))]
    fn route_points(
        &self,
        start: &Point,
        finish: &Point,
    ) -> Option<(Weight, Vec<Node<Self::Entry>>)> {
        let start_node = self.nearest_node(start)?;
        let finish_node = self.nearest_node(finish)?;

        self.route_nodes(start_node.id, finish_node.id)
    }
}

impl<T> Route for Arc<T>
where
    T: Route,
{
    fn route_nodes(
        &self,
        start: Self::Entry,
        finish: Self::Entry,
    ) -> Option<(Weight, Vec<Node<Self::Entry>>)> {
        (**self).route_nodes(start, finish)
    }
}
