use routers_network::{Edge, Ery, Node, network::Discovery};

use geo::Point;
#[cfg(feature = "tracing")]
use tracing::Level;

/// Trait containing utility functions to find nodes and edges upon a root structure.
pub trait Scan<E>: Discovery<E>
where
    E: Ery,
{
    /// A function which returns an unsorted iterator of [`Node`] references which are within
    /// the provided `distance` of the input [point](Point).
    ///
    /// ### Note
    /// This function implements a square-scan.
    ///
    /// Therefore, it bounds the search to be within a square-radius of the origin. Therefore,
    /// it may not select every node within the supplied distance, or it may select more nodes.
    /// This resolution method is however significantly cheaper than a circular scan, so a wider
    /// or shorter search radius may be required in some use-cases.
    fn scan_nodes<'a>(&'a self, point: &Point, distance: f64) -> impl Iterator<Item = &'a Node<E>>
    where
        E: 'a,
    {
        let aabb = square_box(point, distance);
        self.nodes_in_box(aabb)
    }

    /// A function which returns an unsorted iterator of [`FatEdge`] references which are within
    /// the provided `distance` of the input [point](Point).
    ///
    /// ### Note
    /// This function implemEs a square-scan.
    ///
    /// Therefore, it bounds the search to be within a square-radius of the origin. Therefore,
    /// it may not select every node within the supplied distance, or it may select more nodes.
    /// This resolution method is however significantly cheaper than a circular scan, so a wider
    /// or shorter search radius may be required in some use-cases.
    fn scan_edges<'a>(
        &'a self,
        point: &Point,
        distance: f64,
    ) -> impl Iterator<Item = &'a Edge<Node<E>>>
    where
        E: 'a,
    {
        let aabb = square_box(point, distance);
        self.edges_in_box(aabb)
    }

    /// Searches for, and returns a reference to nearest node from the origin [point](Point).
    /// This node may not exist, and therefore the return type is optional.
    fn scan_node<'a>(&'a self, point: Point) -> Option<&'a Node<E>>
    where
        E: 'a {
            self.nodes
        }

    /// Returns an iterator over [`Projected`] nodes on each edge within the specified `distance`.
    /// It does so using the [`Scan::nearest_edges`] function.
    ///
    /// ### Note
    /// This is achieved by creating a line from every edge in the iteration, and finding
    /// the closest point upon that line to the source [point](Point).
    /// This is a bounded projection.
    ///
    /// [`Projected`]: https://en.wikipedia.org/wiki/Projection_(linear_algebra)
    fn scan_nodes_projected<'a>(
        &'a self,
        point: &Point,
        distance: f64,
    ) -> impl Iterator<Item = (Point, &'a Edge<Node<E>>)>
    where
        E: 'a;
}
