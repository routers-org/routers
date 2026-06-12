use std::sync::Arc;

use geo::{Destination, Geodesic, Point};
use rstar::AABB;

use crate::{Edge, Entry, Node};

pub trait Discovery<E: Entry> {
    /// Returns an iterator of *owned* `Edge<Node<E>>` values whose endpoints
    /// fall within the given AABB. Owned (not borrowed) so implementations
    /// can materialise edges on the fly from sparser representations — this
    /// avoids holding a redundant `RTree<Edge<Node<E>>>` alongside the
    /// node-RTree and graph adjacency, which roughly halves runtime RAM at
    /// the cost of two hash lookups per result.
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = Edge<Node<E>>> + Send + 'a>
    where
        E: 'a;

    /// TODO: Document
    fn nodes_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a>
    where
        E: 'a;

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
    fn nodes_at_distance<'a>(
        &'a self,
        point: &Point,
        distance: f64,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a>
    where
        E: 'a,
    {
        let aabb = square_box(point, distance);
        self.nodes_in_box(aabb)
    }

    /// Owned-iterator counterpart to [`Discovery::edges_in_box`]. See its
    /// doc-comment for why this yields by-value rather than by-reference.
    fn edges_at_distance<'a>(
        &'a self,
        point: &Point,
        distance: f64,
    ) -> Box<dyn Iterator<Item = Edge<Node<E>>> + Send + 'a>
    where
        E: 'a,
    {
        let aabb = square_box(point, distance);
        self.edges_in_box(aabb)
    }

    fn node(&self, id: &E) -> Option<&Node<E>>;
    fn edge(&self, source: &E, target: &E) -> Option<Edge<E>>;
}

// Forward through `Arc<T>` so shard-managed networks can be held behind
// an `Arc` without losing trait-method access.
impl<T, E> Discovery<E> for Arc<T>
where
    T: Discovery<E>,
    E: Entry,
{
    fn edges_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = Edge<Node<E>>> + Send + 'a>
    where
        E: 'a,
    {
        (**self).edges_in_box(aabb)
    }

    fn nodes_in_box<'a>(
        &'a self,
        aabb: AABB<Point>,
    ) -> Box<dyn Iterator<Item = &'a Node<E>> + Send + 'a>
    where
        E: 'a,
    {
        (**self).nodes_in_box(aabb)
    }

    fn node(&self, id: &E) -> Option<&Node<E>> {
        (**self).node(id)
    }

    fn edge(&self, source: &E, target: &E) -> Option<Edge<E>> {
        (**self).edge(source, target)
    }
}

fn square_box(point: &Point, square_radius: f64) -> AABB<Point> {
    let bottom_right = Geodesic.destination(*point, 135.0, square_radius);
    let top_left = Geodesic.destination(*point, 315.0, square_radius);

    AABB::from_corners(top_left, bottom_right)
}
