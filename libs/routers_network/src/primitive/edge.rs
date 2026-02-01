use crate::graph::{Graph, Weight};
use crate::primitive::{Direction, Node};
use crate::traits::{Entry, Metadata};
use core::cmp::Ordering;
use core::fmt::Debug;
use geo::Point;
use rstar::AABB;
use serde::Serialize;

/// Represents an edge within the system, along with the directionality of the edge.
///
/// Since the transition graph is a directed graph, it does not support bidirectional edges.
/// Meaning, any edge which is bidirectional must therefore be converted into two edges, each
/// with a different direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
// TODO: Restructure, Rename or Revisit (Confusing)
pub struct DirectionAwareEdgeId<E>
where
    E: Entry,
{
    id: E,
    direction: Direction,
}

impl<E> DirectionAwareEdgeId<E>
where
    E: Entry,
{
    pub fn new(id: E) -> Self {
        Self {
            id,
            direction: Direction::Outgoing,
        }
    }

    /// The [`EdgeIx`] of the direction-aware edge.
    pub fn index(&self) -> E {
        self.id
    }

    /// If the direction-aware edge is forward-facing.
    pub fn forward(self) -> Self {
        DirectionAwareEdgeId {
            direction: Direction::Outgoing,
            ..self
        }
    }

    /// If the direction-aware edge is rear/backward-facing.
    pub fn backward(self) -> Self {
        DirectionAwareEdgeId {
            direction: Direction::Incoming,
            ..self
        }
    }

    #[inline]
    pub const fn direction(&self) -> Direction {
        self.direction
    }
}

impl<E> Ord for DirectionAwareEdgeId<E>
where
    E: Entry,
{
    fn cmp(&self, other: &Self) -> Ordering {
        match self.id.cmp(&other.id) {
            Ordering::Equal => self.direction.cmp(&other.direction),
            ord => ord,
        }
    }
}

impl<E> PartialOrd for DirectionAwareEdgeId<E>
where
    E: Entry,
{
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub struct Edge<E>
where
    E: Entry,
{
    pub source: E,
    pub target: E,
    pub weight: Weight,
    pub id: DirectionAwareEdgeId<E>,
}

impl<E> Edge<E>
where
    E: Entry,
{
    pub const fn id(&self) -> &E {
        &self.id.id
    }

    /// Upsizes a [`Edge`] into a [`FatEdge`].
    #[inline]
    pub fn fatten<M: Metadata>(&self, graph: &Graph<E, M>) -> Option<FatEdge<E>> {
        Some(FatEdge {
            source: *graph.hash.get(&self.source)?,
            target: *graph.hash.get(&self.target)?,
            id: self.id,
            weight: self.weight,
        })
    }
}

impl<'a, E> From<(E, E, &'a (Weight, DirectionAwareEdgeId<E>))> for Edge<E>
where
    E: Entry,
{
    #[inline]
    fn from((source, target, edge): (E, E, &'a (Weight, DirectionAwareEdgeId<E>))) -> Self {
        Edge {
            source,
            target,
            weight: edge.0,
            id: edge.1,
        }
    }
}

/// Represents a fat edge within the system.
///
/// A [`FatEdge`], unlike an [`Edge`] contains source/target information inside the structure
/// itself, instead of through [`NodeIx`] indirection. This makes the structure "fat" since
/// the [`Node`] struct is large.
///
/// A helper method, [`FatEdge::thin`] is provided to downsize to an [`Edge`]. Note this process
/// is lossy if no data source containing the original node is present.
///
/// ### Note
///
/// As it is large, this should only be used transitively
/// like in [`Scan::nearest_edges`](crate::route::Scan::nearest_edges).
#[derive(Debug, Serialize)]
pub struct FatEdge<E>
where
    E: Entry,
{
    pub source: Node<E>,
    pub target: Node<E>,

    pub weight: Weight,
    pub id: DirectionAwareEdgeId<E>,
}

impl<E> FatEdge<E>
where
    E: Entry,
{
    pub const fn id(&self) -> &E {
        &self.id.id
    }

    /// Downsizes a [`FatEdge`] to an [`Edge`].
    #[inline]
    pub fn thin(&self) -> Edge<E> {
        Edge {
            source: self.source.id,
            target: self.target.id,
            id: self.id,
            weight: self.weight,
        }
    }
}

impl<E> rstar::RTreeObject for FatEdge<E>
where
    E: Entry,
{
    type Envelope = AABB<Point>;

    fn envelope(&self) -> Self::Envelope {
        AABB::from_corners(self.target.position, self.source.position)
    }
}
