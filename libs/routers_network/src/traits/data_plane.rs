//! The read-only data layer of a routing network.
//!
//! [`DataPlane`] captures the bits a consumer needs to look up nodes,
//! ways and the graph topology *by identifier*. It deliberately knows
//! nothing about routing or spatial queries — those live on the
//! [`Route`](crate::Route), [`Scan`](crate::Scan) and
//! [`Discovery`](crate::Discovery) traits.
//!
//! **Associated types vs. generics.** `DataPlane` exposes its `Entry` and
//! `Metadata` types as associated types (`type Entry`, `type Meta`) rather
//! than trait-level generics. This means downstream consumers — viewers,
//! exporters — can bound on `N: DataPlane` alone and pick the concrete
//! `N::Entry` / `N::Meta` off the type, instead of threading `<E, M, N>`
//! through every signature. The full [`Network`](crate::Network) trait is
//! still parameterised on `<E, M>` for backwards compatibility; the two
//! styles bridge via the blanket impl in `network.rs`.

use core::fmt::Debug;
use std::sync::Arc;

use crate::{DirectionAwareEdgeId, Edge, Entry, Metadata, Node, edge::Weight};
use geo::Point;

pub type EdgeData<E> = (Weight, DirectionAwareEdgeId<E>);
pub type GraphEdge<E> = (E, E, EdgeData<E>);

/// Read-only access to a routing network's nodes, ways and topology.
///
/// Implementors are typically concrete graph storage (e.g. `OsmNetwork`,
/// `ShardedNetwork`). Composing them into the full [`Network`](crate::Network)
/// trait is a matter of also implementing
/// [`Scan`](crate::Scan) (nearest-neighbour) and [`Route`](crate::Route)
/// (shortest path).
pub trait DataPlane: Debug + Send + Sync {
    type Entry: Entry;
    type Meta: Metadata;

    fn metadata(&self, id: &Self::Entry) -> Option<&Self::Meta>;

    fn point(&self, id: &Self::Entry) -> Option<Point>;

    fn edges_outof<'a>(
        &'a self,
        id: Self::Entry,
    ) -> Box<dyn Iterator<Item = GraphEdge<Self::Entry>> + 'a>;
    fn edges_into<'a>(
        &'a self,
        id: Self::Entry,
    ) -> Box<dyn Iterator<Item = GraphEdge<Self::Entry>> + 'a>;

    /// Produces an iterator of points for a given input.
    ///
    /// All provided nodes that do not exist will not be returned, so the iterator's
    /// length may be smaller than the input slice.
    fn line(&self, nodes: &[Self::Entry]) -> Vec<Point> {
        nodes.iter().filter_map(|node| self.point(node)).collect()
    }

    fn fatten(&self, edge: &Edge<Self::Entry>) -> Option<Edge<Node<Self::Entry>>>;
}

// Blanket forward through `Arc<T>` so consumers (e.g. a viewer that swaps
// the inner network as shards load) can hold their network behind an
// `Arc` without losing trait-method access. The body of each call deref's
// through the Arc to T's own impl.
impl<T> DataPlane for Arc<T>
where
    T: DataPlane,
{
    type Entry = <T as DataPlane>::Entry;
    type Meta = <T as DataPlane>::Meta;

    fn metadata(&self, id: &Self::Entry) -> Option<&Self::Meta> {
        (**self).metadata(id)
    }

    fn point(&self, id: &Self::Entry) -> Option<Point> {
        (**self).point(id)
    }

    fn edges_outof<'a>(
        &'a self,
        id: Self::Entry,
    ) -> Box<dyn Iterator<Item = GraphEdge<Self::Entry>> + 'a> {
        (**self).edges_outof(id)
    }

    fn edges_into<'a>(
        &'a self,
        id: Self::Entry,
    ) -> Box<dyn Iterator<Item = GraphEdge<Self::Entry>> + 'a> {
        (**self).edges_into(id)
    }

    fn line(&self, nodes: &[Self::Entry]) -> Vec<Point> {
        (**self).line(nodes)
    }

    fn fatten(&self, edge: &Edge<Self::Entry>) -> Option<Edge<Node<Self::Entry>>> {
        (**self).fatten(edge)
    }
}
