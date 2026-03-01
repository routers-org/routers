use geo::{Haversine, InterpolatableLine, Line, LineLocatePoint, Point};

use crate::{Discovery, Edge, Entry, Node};

/// Trait containing utility functions to find nodes on a root structure.
pub trait Scan<E>: Discovery<E>
where
    E: Entry,
{
    /// Searches for, and returns a reference to nearest node from the origin [point](Point).
    /// This node may not exist, and therefore the return type is optional.
    fn nearest_node<'a>(&'a self, point: &Point) -> Option<&'a Node<E>>
    where
        E: 'a;

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
    ) -> Box<dyn Iterator<Item = (Point, &'a Edge<Node<E>>)> + Send + 'a>
    where
        E: 'a,
    {
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
                        .map(|point| (point, edge))
                }),
        )
    }
}
