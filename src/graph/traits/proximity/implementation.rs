use crate::FatEdge;
use crate::graph::Graph;
use crate::graph::Scan;

use routers_codec::primitive::Node;
use routers_codec::{Entry, Metadata};

use geo::{Destination, Geodesic, Haversine, InterpolatableLine, Line, LineLocatePoint, Point};
use rstar::AABB;

#[cfg(feature = "tracing")]
use tracing::Level;

impl<E, M> Scan<E> for Graph<E, M>
where
    E: Entry,
    M: Metadata,
{
    #[cfg_attr(feature = "tracing", tracing::instrument(level = Level::INFO, skip(self)))]
    #[inline]
    fn scan_nodes<'a>(&'a self, point: &Point, distance: f64) -> impl Iterator<Item = &'a Node<E>>
    where
        E: 'a,
    {
        let bounding_box = square_box(point, distance);
        self.index().locate_in_envelope(&bounding_box)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = Level::INFO, skip(self)))]
    #[inline]
    fn scan_edges<'a>(
        &'a self,
        point: &Point,
        distance: f64,
    ) -> impl Iterator<Item = &'a FatEdge<E>>
    where
        E: 'a,
    {
        let bounding_box = square_box(point, distance);
        self.index_edge().locate_in_envelope(&bounding_box)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = Level::INFO, skip(self)))]
    #[inline]
    fn scan_node<'a>(&'a self, point: Point) -> Option<&'a Node<E>>
    where
        E: 'a,
    {
        self.index.nearest_neighbor(&point)
    }

    #[cfg_attr(feature = "tracing", tracing::instrument(level = Level::INFO))]
    #[inline]
    fn scan_nodes_projected<'a>(
        &'a self,
        point: &Point,
        distance: f64,
    ) -> impl Iterator<Item = (Point, &'a FatEdge<E>)>
    where
        E: 'a,
    {
        // Total overhead of this function is negligible.
        self.scan_edges(point, distance).filter_map(move |edge| {
            let line = Line::new(edge.source.position, edge.target.position);

            // We locate the point upon the linestring,
            // and then project that fractional (%)
            // upon the linestring to obtain a point
            line.line_locate_point(point)
                .map(|frac| line.point_at_ratio_from_start(&Haversine, frac))
                .map(|point| (point, edge))
        })
    }
}

fn square_box(point: &Point, square_radius: f64) -> AABB<Point> {
    let bottom_right = Geodesic.destination(*point, 135.0, square_radius);
    let top_left = Geodesic.destination(*point, 315.0, square_radius);

    AABB::from_corners(top_left, bottom_right)
}
