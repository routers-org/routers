use crate::transition::candidate::*;
use routers_codec::{Entry, Metadata};
use std::ops::Deref;

use crate::Graph;
use geo::Point;

/// A route representing the parsed output from a function
/// passed through the transition graph.
pub struct RoutedPath<E, M>
where
    E: Entry,
    M: Metadata,
{
    /// The exactly-routed elements.
    ///
    /// For a map-match request, these are the values which line up with the inputs
    /// for a one-to-one match. I.e. there is a discretized point for every input point.
    pub discretized: Path<E, M>,

    /// The interpolated elements.
    ///
    /// These points are the full interpreted trip, consisting of every turn and roadway
    /// the algorithm has assumed as a part of the path taken. This is useful for visualising
    /// a trip by "recovering" lost information, or understanding subtle details such as
    /// when the route left or joined a highway.
    pub interpolated: Path<E, M>,
}

impl<E, M> RoutedPath<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn new(collapsed_path: CollapsedPath<E>, graph: &Graph<E, M>) -> Self {
        // Collect the collapsed route, providing graph context.
        let discretized = collapsed_path
            .route
            .iter()
            .flat_map(|id| collapsed_path.candidates.candidate(id))
            .flat_map(|candidate| PathElement::new(candidate, graph))
            .collect::<Path<E, M>>();

        // Collect and interpolate required information from the
        // collapsed path. Derives routing information for a
        // informative response.
        let interpolated = collapsed_path
            .interpolated
            .into_iter()
            .flat_map(|reachable| reachable.path)
            .flat_map(|edge| edge.fatten(graph))
            .flat_map(|edge| PathElement::from_fat(edge, graph))
            .collect::<Path<E, M>>();

        RoutedPath {
            discretized,
            interpolated,
        }
    }
}

/// A representation of a path taken.
/// Consists of an array of [PathElement]s, containing relevant information for positioning.
pub struct Path<E, M>
where
    E: Entry,
    M: Metadata,
{
    /// The elements which construct the path.
    elements: Vec<PathElement<E, M>>,
}

impl<E, M> FromIterator<PathElement<E, M>> for Path<E, M>
where
    E: Entry,
    M: Metadata,
{
    fn from_iter<I: IntoIterator<Item = PathElement<E, M>>>(iter: I) -> Self {
        let elements = iter.into_iter().collect::<Vec<_>>();

        Path { elements }
    }
}

impl<E, M> Deref for Path<E, M>
where
    E: Entry,
    M: Metadata,
{
    type Target = Vec<PathElement<E, M>>;

    fn deref(&self) -> &Self::Target {
        &self.elements
    }
}

/// An element within a path, consisting of the [Point] the
/// element represents within the path, as well as metadata (Meta)
/// for the path element, and the edge within the source network at
/// which the element exists.
pub struct PathElement<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub point: Point,
    pub edge: FatEdge<E>,

    pub metadata: M,
}

impl<E, M> PathElement<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn new(candidate: Candidate<E>, graph: &Graph<E, M>) -> Option<Self> {
        Some(PathElement {
            point: candidate.position,
            edge: candidate.edge.fatten(graph)?,
            metadata: graph.meta.get(candidate.edge.id())?.clone(),
        })
    }

    pub fn from_fat(edge: FatEdge<E>, graph: &Graph<E, M>) -> Option<Self> {
        Some(PathElement {
            point: edge.source.position,
            metadata: graph.meta.get(edge.id())?.clone(),
            edge,
        })
    }
}
