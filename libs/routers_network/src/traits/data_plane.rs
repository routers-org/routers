//! The read-only data layer of a routing network.
//!
//! [`DataPlane`] captures the bits a consumer needs to look up nodes,
//! ways and the graph topology *by identifier*. It deliberately knows
//! nothing about routing or spatial queries — those live on the
//! [`Route`](crate::Route), [`Scan`](crate::Scan) and
//! [`Discovery`](crate::Discovery) traits.
//!
//! **Associated types, not generics.** `DataPlane` exposes its `Entry` and
//! `Metadata` types as associated types (`type Entry`, `type Meta`) rather
//! than trait-level generics. This means downstream consumers — viewers,
//! exporters — can bound on `N: DataPlane` alone and pick the concrete
//! `N::Entry` / `N::Meta` / `N::Runtime` off the type, instead of threading
//! `<E, M, N>` through every signature. The full [`Network`](crate::Network)
//! trait follows the same style, inheriting these associated types via the
//! blanket impl in `network.rs`.

use alloc::sync::Arc;
use core::fmt::Debug;

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
    /// The runtime configuration type of this plane's metadata.
    ///
    /// Declared here (and tied to `Meta` by the bound below) so consumers can
    /// bound and name `N::Runtime` without reaching into the [`Metadata`]
    /// trait themselves.
    type Runtime: Clone + Debug + Send + Sync + PartialEq;
    type Meta: Metadata<Runtime = Self::Runtime>;

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

    /// The matcher's bounded reachability search from `root`, run by the
    /// network itself: every node settled before the search first settles
    /// one at `reach` metres or beyond, each with the node it was reached
    /// from (the root with the default entry).
    ///
    /// `None` — the default — leaves the search to the matcher, which walks
    /// [`edges_outof`](Self::edges_outof). A network whose storage can run
    /// it faster may answer instead, and must then settle exactly the nodes,
    /// with exactly the parents, that the matcher's own search would.
    fn reach(
        &self,
        _root: &Self::Entry,
        _reach: f64,
        _runtime: &Self::Runtime,
    ) -> Option<Vec<(Self::Entry, Self::Entry)>> {
        None
    }
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
    type Runtime = <T as DataPlane>::Runtime;
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

    fn reach(
        &self,
        root: &Self::Entry,
        reach: f64,
        runtime: &Self::Runtime,
    ) -> Option<Vec<(Self::Entry, Self::Entry)>> {
        (**self).reach(root, reach, runtime)
    }
}
