use crate::transition::candidate::*;
use core::ops::Deref;
use routers_network::{Edge, Entry, Metadata, Network, Node};
use serde::Serialize;

use geo::Coord;

/// A route representing the parsed output from a function
/// passed through the transition graph.
#[derive(Serialize, Debug)]
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
    pub fn new(collapsed_path: CollapsedPath<E>, network: &impl Network<E, M>) -> Self {
        // Collect matched candidates in order.  Virtual start/end nodes have no
        // lookup entry so flat_map quietly skips them.
        let matched: Vec<Candidate<E>> = collapsed_path
            .route
            .iter()
            .flat_map(|id| collapsed_path.candidates.candidate(id))
            .collect();

        // discretized: one PathElement per GPS input point.
        let discretized = matched
            .iter()
            .flat_map(|c| PathElement::new(*c, network))
            .collect::<Path<E, M>>();

        // interpolated: the complete traversed path — each candidate edge
        // interleaved with the routing edges that bridge consecutive candidates.
        //
        // For each reachable[i] (transition from candidate i to candidate i+1):
        //   1. Emit candidate i's own edge.
        //   2. Emit all intermediate routing edges (source.edge.target → target.edge.source).
        // After all transitions, emit the last candidate's edge.
        //
        // Consecutive identical (source, target) pairs are deduplicated so that
        // distance-only transitions (same directed edge for both candidates) do not
        // repeat the segment.  The key is the node-pair rather than the way ID so
        // that multiple directed segments of the same long OSM way are all retained.
        let interpolated = {
            let reachables = collapsed_path.interpolated;
            let mut elements: Vec<PathElement<E, M>> = Vec::new();
            let mut last_edge: Option<(i64, i64)> = None;

            // Emit one edge into `elements`, skipping it if the same directed
            // edge (same source node, same target node) was just emitted.
            let push_edge = |edge: &Edge<E>,
                                 elements: &mut Vec<PathElement<E, M>>,
                                 last_key: &mut Option<(i64, i64)>| {
                let key = (edge.source.identifier(), edge.target.identifier());
                if *last_key == Some(key) {
                    return;
                }
                if let Some(fat) = network.fatten(edge) {
                    if let Some(pe) = PathElement::from_fat(fat, network) {
                        elements.push(pe);
                        *last_key = Some(key);
                    }
                }
            };

            for (i, reachable) in reachables.iter().enumerate() {
                // Emit the source candidate's edge.
                if let Some(source) = matched.get(i) {
                    push_edge(&source.edge, &mut elements, &mut last_edge);
                }
                // Emit any intermediate routing edges between the candidates.
                for edge in &reachable.path {
                    push_edge(edge, &mut elements, &mut last_edge);
                }
            }

            // Emit the final candidate's edge.
            if let Some(last_cand) = matched.last() {
                push_edge(&last_cand.edge, &mut elements, &mut last_edge);
            }

            Path { elements }
        };

        RoutedPath {
            discretized,
            interpolated,
        }
    }
}

/// A representation of a path taken.
/// Consists of an array of [PathElement]s, containing relevant information for positioning.
#[derive(Debug, Serialize)]
pub struct Path<E, M>
where
    E: Entry,
    M: Metadata,
{
    /// The elements which construct the path.
    pub elements: Vec<PathElement<E, M>>,
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
#[derive(Debug, Serialize)]
pub struct PathElement<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub point: Coord,
    pub edge: Edge<Node<E>>,

    pub metadata: M,
}

impl<E, M> PathElement<E, M>
where
    E: Entry,
    M: Metadata,
{
    pub fn new(candidate: Candidate<E>, network: &impl Network<E, M>) -> Option<Self> {
        Some(PathElement {
            point: candidate.position.0,
            edge: network.fatten(&candidate.edge)?,
            metadata: network.metadata(candidate.edge.id())?.clone(),
        })
    }

    pub fn from_fat(edge: Edge<Node<E>>, network: &impl Network<E, M>) -> Option<Self> {
        Some(PathElement {
            point: edge.source.position.0,
            metadata: network.metadata(edge.id())?.clone(),
            edge,
        })
    }
}
