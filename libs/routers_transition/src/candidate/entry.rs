use crate::{candidate::CandidateRef, primitives::RoutingContext};

use core::fmt::Debug;
use geo::{Bearing, Distance, Haversine, LineLocatePoint, LineString, Point};
use routers_network::{Edge, Entry, Metadata, Network};
use serde::{Deserialize, Serialize};

/// One possible anchoring of a trajectory point: an edge of the network, the
/// projected [`position`](Self::position) along it, and the
/// [`emission`](Self::emission) cost of choosing it.
///
/// Candidates are produced per point by a
/// [`LayerGeneration`](crate::generation::LayerGeneration) and stored in a
/// [`CandidateStore`](crate::CandidateStore); [`location`](Self::location)
/// is the candidate's
/// positional identity within that store (and the trellis), stamped when its
/// layer is pushed.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct Candidate<E>
where
    E: Entry,
{
    /// Refers to the points within the map graph (Underlying routing structure)
    pub edge: Edge<E>,
    pub position: Point,
    pub emission: u32,

    pub location: CandidateRef,
}

/// A virtual tail is a representation of the distance from some intermediary point
/// on a candidate edge to the edge's end. This is used to resolve routing decisions
/// within short distances, in which case we need to understand the distance between
/// our intermediary projected position and some end of the edge.
///
/// If the candidates were on the same edge, we would instead utilise the
/// [`ResolutionMethod`](crate::ResolutionMethod) option.
///
/// The below diagram images the virtual tail for intermediate candidate position.
/// For example, the [`VirtualTail::ToSource`] variant can be seen to measure the
/// distance from this intermediate, to the source of the edge, and vice versa for
/// the target.
///
/// ```text
///                 Candidate
///          ToSource   |   ToTarget
///        +------------|------------+
///      Source                    Target
/// ```
pub enum VirtualTail {
    /// The distance from the edge's source to the virtual candidate position.
    ToSource,

    /// The distance from the virtual candidate position to the edge target.
    ToTarget,
}

impl<E> Candidate<E>
where
    E: Entry,
{
    /// Returns the percentage of the distance through the edge, relative to the position
    /// upon the linestring by which it lies.
    ///
    /// The below diagram visualises this percentage. Note that `0%` represents
    /// an intermediate which is equivalent to the source of the edge, whilst `100%`
    /// represents an intermediate equivalent to the target.
    ///
    /// ```text
    ///                Edge Percentages
    ///     Source                         Target
    ///       +---------|----------------|---+
    ///                0.4              0.9
    ///               (40%)            (90%)
    /// ```
    ///
    pub fn percentage<M: Metadata>(&self, graph: &dyn Network<E, M>) -> Option<f64> {
        let edge = graph
            .line(&[self.edge.source, self.edge.target])
            .into_iter()
            .collect::<LineString>();

        edge.line_locate_point(&self.position)
    }

    /// Whether `other` is reachable from `self` by travelling along their shared
    /// edge alone — i.e. both sit on the same directed edge with `other` at or
    /// ahead of `self`. `None` when the shared edge is too degenerate to locate a
    /// position on. A `Some(false)` covers candidates on different edges (which
    /// must be routed between) and same-edge back-tracking.
    pub fn directly_reachable<M: Metadata>(
        &self,
        other: &Candidate<E>,
        graph: &dyn Network<E, M>,
    ) -> Option<bool> {
        if self.edge.id.index() != other.edge.id.index() {
            return Some(false);
        }

        let same_direction =
            self.edge.source == other.edge.source && self.edge.target == other.edge.target;
        let ahead = self.percentage(graph)? <= other.percentage(graph)?;

        Some(same_direction && ahead)
    }

    /// Get the bearing of the candidate's edge (source endpoint -> target endpoint).
    pub fn edge_heading<M, N>(&self, ctx: &RoutingContext<E, M, N>) -> Option<f64>
    where
        M: Metadata,
        N: Network<E, M>,
    {
        let s = ctx.map.point(&self.edge.source)?;
        let t = ctx.map.point(&self.edge.target)?;

        // Consider degenerate case
        if Haversine.distance(s, t) < 1.0 {
            return None;
        }

        Some(Haversine.bearing(s, t))
    }

    /// Calculates the offset, in meters, of the candidate to it's edge by the [`VirtualTail`].
    pub fn offset<N, M: Metadata>(
        &self,
        ctx: &RoutingContext<E, M, N>,
        variant: VirtualTail,
    ) -> Option<f64>
    where
        N: Network<E, M>,
    {
        match variant {
            VirtualTail::ToSource => {
                let source = ctx.map.point(&self.edge.source)?;
                Some(Haversine.distance(source, self.position))
            }
            VirtualTail::ToTarget => {
                let target = ctx.map.point(&self.edge.target)?;
                Some(Haversine.distance(self.position, target))
            }
        }
    }

    pub fn new(edge: Edge<E>, position: Point, emission: u32, location: CandidateRef) -> Self {
        Self {
            edge,
            position,
            emission,
            location,
        }
    }
}
