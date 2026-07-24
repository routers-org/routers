use alloc::sync::Arc;

use geo::{Haversine, InterpolatableLine, Line, LineLocatePoint, Point};

use crate::{Discovery, Edge, Node};

/// Trait containing utility functions to find nodes on a root structure.
pub trait Scan: Discovery {
    /// Searches for, and returns a reference to nearest node from the origin [point](Point).
    /// This node may not exist, and therefore the return type is optional.
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<Self::Entry>>;

    /// Returns an iterator over [`Projected`] nodes on each edge within the specified `distance`.
    /// It does so using the [`Scan::nearest_edges`] function.
    ///
    /// ### Note
    /// This is achieved by creating a line from every edge in the iteration, and finding
    /// the closest point upon that line to the source [point](Point).
    /// This is a bounded projection.
    ///
    /// [`Projected`]: https://en.wikipedia.org/wiki/Projection_(linear_algebra)
    fn nearest_nodes_projected<'a>(
        &'a self,
        point: &'a Point,
        distance: f64,
    ) -> Box<dyn Iterator<Item = (Point, Edge<Node<Self::Entry>>)> + Send + 'a> {
        // Total overhead of this function is negligible.
        Box::new(
            self.edges_at_distance(point, distance)
                .into_iter()
                .filter_map(move |edge| {
                    let line = Line::new(edge.source.position, edge.target.position);

                    // We locate the point upon the linestring,
                    // and then project that fractional (%)
                    // upon the linestring to obtain a point
                    line.line_locate_point(point)
                        .map(|frac| line.point_at_ratio_from_start(&Haversine, frac))
                        .zip(Some(edge))
                }),
        )
    }
}

impl<T> Scan for Arc<T>
where
    T: Scan,
{
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<Self::Entry>> {
        (**self).nearest_node(point)
    }
}
