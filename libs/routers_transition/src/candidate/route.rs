use crate::{candidate::*, primitives::ResolutionMethod};
use core::ops::Deref;
use routers_network::{Edge, Entry, Metadata, Network, Node};
use serde::{Deserialize, Serialize};

use geo::Coord;

/// The result of a facade-level match: the trajectory resolved onto the
/// network, ready to render or persist.
///
/// Two views of the same match are provided, and you will usually want one or
/// the other. [`discretized`](Self::discretized) answers *where was each
/// input point, really?* — one element per input point, in order.
/// [`interpolated`](Self::interpolated) answers *which roads were driven?* —
/// the full path including every turn and roadway between the matched points,
/// recovering what the trajectory's sample rate lost.
///
/// Every element carries its network edge and metadata, so nothing further
/// needs to be looked up against the map downstream.
#[derive(Serialize, Deserialize, Debug)]
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
    pub fn new(collapsed_path: CollapsedPath<'_, E>, network: &impl Network<Entry = E, Meta = M>) -> Self {
        // Collect matched candidates in order.  Virtual start/end nodes have no
        // lookup entry so flat_map quietly skips them.
        let matched: Vec<Candidate<E>> = collapsed_path
            .route
            .iter()
            .flat_map(|id| collapsed_path.candidates.candidate(id))
            .collect();

        // One PathElement per GPS input point.
        let discretized = matched
            .iter()
            .flat_map(|c| PathElement::new(*c, network))
            .collect::<Path<E, M>>();

        // The complete traversed path. Each candidate edge is interleaved
        // with the routing edges that bridge consecutive candidates.
        //
        // We iterate through all discrete elements and ensure to include the
        // edges bridging them (intermediate edges). So as to conjoin the source's
        // target and the target's source with all relevant filler, preventing seemingly
        // jumpy segment joins for much-dispersed traffic, or low-frequency positions.
        //
        // Consecutive identical ends are deduplicated so that distance-only transitions,
        // which have the same directed edge for both candidates, do not repeat the segment.
        let interpolated = {
            let mut elements = Vec::new();

            // Include initial edge source
            if let Some(first) = matched.first() {
                if let Some(fat) = network.fatten(&first.edge) {
                    if let Some(pe) = PathElement::from_edge_source(fat, network) {
                        elements.push(pe);
                    }
                }
            }

            for (i, reachable) in collapsed_path.interpolated.iter().enumerate() {
                let current = &matched[i];

                // Add current candidate position
                if let Some(pe) = PathElement::new(*current, network) {
                    elements.push(pe);
                }

                if let ResolutionMethod::Standard = reachable.resolution_method {
                    // Add target of current candidate edge
                    if let Some(fat) = network.fatten(&current.edge) {
                        if let Some(pe) = PathElement::from_edge_target(fat, network) {
                            elements.push(pe);
                        }
                    }

                    // Add intermediate edges
                    for edge in &reachable.path {
                        if let Some(fat) = network.fatten(edge) {
                            if let Some(pe) = PathElement::from_edge_source(fat, network) {
                                elements.push(pe);
                            }
                        }
                    }

                    // Add source of next candidate edge
                    if let Some(next) = matched.get(i + 1) {
                        if let Some(fat) = network.fatten(&next.edge) {
                            if let Some(pe) = PathElement::from_edge_source(fat, network) {
                                elements.push(pe);
                            }
                        }
                    }
                }
            }

            // Add the very last candidate position
            if let Some(last_candidate) = matched.last()
                && let Some(pe) = PathElement::new(*last_candidate, network)
            {
                elements.push(pe);
            }

            elements.dedup_by(|a, b| a.point == b.point);

            Path { elements }
        };

        RoutedPath {
            discretized,
            interpolated,
        }
    }
}

/// An ordered series of [`PathElement`]s describing a path over the network.
///
/// Dereferences to its `elements`, so it can be iterated directly.
#[derive(Debug, Serialize, Deserialize)]
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

/// One position along a [`Path`]: the point itself, the network edge it lies
/// on, and that edge's metadata.
#[derive(Debug, Serialize, Deserialize)]
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
    pub fn new(candidate: Candidate<E>, network: &impl Network<Entry = E, Meta = M>) -> Option<Self> {
        Some(PathElement {
            point: candidate.position.0,
            edge: network.fatten(&candidate.edge)?,
            metadata: network.metadata(candidate.edge.id())?.clone(),
        })
    }

    pub fn from_edge_source(edge: Edge<Node<E>>, network: &impl Network<Entry = E, Meta = M>) -> Option<Self> {
        Some(PathElement {
            point: edge.source.position.0,
            metadata: network.metadata(edge.id())?.clone(),
            edge,
        })
    }

    pub fn from_edge_target(edge: Edge<Node<E>>, network: &impl Network<Entry = E, Meta = M>) -> Option<Self> {
        Some(PathElement {
            point: edge.target.position.0,
            metadata: network.metadata(edge.id())?.clone(),
            edge,
        })
    }
}
