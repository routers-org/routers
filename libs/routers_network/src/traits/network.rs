use crate::{Entry, Metadata, Scan};
use geo::Point;

pub trait Network<E, M>: Scan<E>
where
    E: Entry,
    M: Metadata,
{
    fn metadata(&self, id: &E) -> Option<&M>;

    fn point(&self, id: &E) -> Option<Point>;

    /// Produces an iterator of points for a given input.
    ///
    /// All provided nodes that do not exist will not be returned, so the iterator's
    /// length may be smaller than the input slice.
    fn line(&self, nodes: &[E]) -> impl Iterator<Item = Point> {
        nodes.iter().filter_map(|node| self.point(node))
    }
}

// impl<T, E: Entry, M: Metadata> Network<E, M> for T where T: Discovery<E> + FullObject<E, M> {}
