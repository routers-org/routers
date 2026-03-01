use crate::traits::Entry;

use core::cmp::{Ord, Ordering};
use core::fmt::Debug;
use core::hash::{Hash, Hasher};
use core::ops::Deref;
use geo::{Destination, Distance, Euclidean, Geodesic, Point};
use rstar::{AABB, Envelope};
use serde::Serialize;

/// The standardised node primitive containing a generic
/// identifier which must implement [Entry], and contain
/// some given [Point].
#[derive(Debug, Copy, Clone, Serialize)]
pub struct Node<E>
where
    E: Entry,
{
    pub id: E,
    pub position: Point,
}

impl<E> Node<E>
where
    E: Entry,
{
    /// Constructs a `Node` from a given `LatLng` and `id`.
    pub fn new(position: Point, id: E) -> Self {
        Self { id, position }
    }

    /// Constructs the rectangular Axis-Aligned Bounding Box ([AABB](rstar::AABB))
    /// for the square [distance](#param.distance) around the node position.
    pub fn bounding(&self, distance: f64) -> AABB<Point> {
        let bottom_right = Geodesic.destination(self.position, 135.0, distance);
        let top_left = Geodesic.destination(self.position, 315.0, distance);
        AABB::from_corners(top_left, bottom_right)
    }
}

impl<E> rstar::PointDistance for Node<E>
where
    E: Entry,
{
    fn distance_2(
        &self,
        point: &<Self::Envelope as Envelope>::Point,
    ) -> <<Self::Envelope as Envelope>::Point as rstar::Point>::Scalar {
        Euclidean.distance(self.position, *point).powi(2)
    }
}

impl<E> rstar::RTreeObject for Node<E>
where
    E: Entry,
{
    type Envelope = AABB<Point>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_point(self.position)
    }
}

impl<E: Entry> Entry for Node<E> {
    fn identifier(&self) -> i64 {
        self.id.identifier()
    }
}

impl<E: Entry> Deref for Node<E> {
    type Target = E;

    fn deref(&self) -> &Self::Target {
        &self.id
    }
}

impl<E: Entry> Default for Node<E> {
    fn default() -> Node<E> {
        Node {
            id: E::default(),
            position: Point::new(0., 0.),
        }
    }
}

impl<E: Entry> Ord for Node<E> {
    fn cmp(&self, other: &Node<E>) -> Ordering {
        self.id.cmp(&other.id)
    }
}

impl<E: Entry> PartialOrd for Node<E> {
    fn partial_cmp(&self, other: &Node<E>) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl<E: Entry> PartialEq for Node<E> {
    fn eq(&self, other: &Node<E>) -> bool {
        self.id.eq(&other.id)
    }
}

impl<E: Entry> Eq for Node<E> {}

impl<E: Entry> Hash for Node<E> {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state)
    }
}
