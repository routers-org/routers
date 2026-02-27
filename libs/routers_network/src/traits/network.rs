use crate::{Edge, Entry, Metadata, Node};
use geo::Point;
use rstar::AABB;

pub trait Network<E, M>: Discovery<E> + FullObject<E, M>
where
    E: Entry,
    M: Metadata,
{
}

impl<T, E: Entry, M: Metadata> Network<E, M> for T where T: Discovery<E> + FullObject<E, M> {}

pub trait Discovery<E: Entry> {
    fn edges_in_box<'a>(&'a self, aabb: AABB<Point>) -> impl Iterator<Item = &'a Edge<E>>
    where
        E: 'a;
    fn nodes_in_box<'a>(&'a self, aabb: AABB<Point>) -> impl Iterator<Item = &'a Node<E>>
    where
        E: 'a;
}

pub trait FullObject<E: Entry, M: Metadata> {
    // fn edge(&self, id: &E) -> Option<&Edge<E>>;
    fn metadata(&self, id: &E) -> Option<&M>;

    fn node(&self, id: &E) -> Option<&Node<E>>;

    fn point(&self, id: &E) -> Option<Point> {
        self.node(id).map(|v| v.position)
    }

    fn line(&self, nodes: &[E]) -> impl Iterator<Item = Point> {
        nodes.iter().filter_map(|node| self.point(node))
    }
}
