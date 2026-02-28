use crate::transition::RoutingContext;

use core::cmp::Ordering;
use core::fmt::Debug;
use core::ops::Add;
use geo::{Distance, Haversine, LineLocatePoint, LineString, Point};
use pathfinding::num_traits::Zero;
use routers_network::{Edge, Entry, Metadata, Network};

/// The location of a candidate within a solution.
/// This identifies which layer the candidate came from, and which node in the layer it was.
///
/// This is useful for debugging purposes to understand a node without requiring further context.
#[derive(Clone, Copy, Debug)]
pub struct CandidateLocation {
    pub layer_id: usize,
    pub node_id: usize,
}

/// Represents the candidate selected within a layer.
///
/// This value holds the [edge](#field.edge) on the underlying routing structure it is sourced
/// from, along with the candidate position, [position](#field.position).
///
/// It further contains the emission cost [emission](#field.emission) associated with choosing this
/// candidate and the candidate's location within the solution, [location](#field.location).
#[derive(Clone, Copy, Debug)]
pub struct Candidate<E>
where
    E: Entry,
{
    /// Refers to the points within the map graph (Underlying routing structure)
    pub edge: Edge<E>,
    pub position: Point,
    pub emission: u32,

    pub location: CandidateLocation,
}

/// A virtual tail is a representation of the distance from some intermediary point
/// on a candidate edge to the edge's end. This is used to resolve routing decisions
/// within short distances, in which case we need to understand the distance between
/// our intermediary projected position and some end of the edge.
///
/// If the candidates were on the same edge, we would instead utilise the
/// [ResolutionMethod] option.
///
/// The below diagram images the virtual tail for intermediate candidate position.
/// For example, the [`VirtualTail::ToSource`] variant can be seen to measure the
/// distance from this intermediate, to the source of the edge, and vice versa for
/// the target.
///
///                 Candidate
///          ToSource   |   ToTarget
///        +------------|------------+
///      Source                    Target
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
    ///                Edge Percentages
    ///     Source                         Target
    ///       +---------|----------------|---+
    ///                0.4              0.9
    ///               (40%)            (90%)
    ///
    pub fn percentage<M: Metadata>(&self, graph: &dyn Network<E, M>) -> Option<f64> {
        let edge = graph
            .line(&[self.edge.source, self.edge.target])
            .into_iter()
            .collect::<LineString>();

        edge.line_locate_point(&self.position)
    }

    /// Calculates the offset, in meters, of the candidate to it's edge by the [`VirtualTail`].
    pub fn offset<M: Metadata>(
        &self,
        ctx: &RoutingContext<E, M>,
        variant: VirtualTail,
    ) -> Option<f64> {
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

    pub fn new(edge: Edge<E>, position: Point, emission: u32, location: CandidateLocation) -> Self {
        Self {
            edge,
            position,
            emission,
            location,
        }
    }
}

/// Represents the edge of this candidate within the candidate graph.
///
/// This is distinct from [`Edge`] since it exists within the candidate graph
/// of the [`Transition`](crate::route::graph::Transition), not of [`Graph`].
///
/// This edge stores the weight associated with traversing this edge.
///
#[derive(Clone, Copy, Debug, Default)]
#[repr(transparent)]
pub struct CandidateEdge {
    pub weight: u32,
}

impl Eq for CandidateEdge {}

impl PartialEq<Self> for CandidateEdge {
    fn eq(&self, other: &Self) -> bool {
        self.weight.eq(&other.weight)
    }
}

impl PartialOrd<Self> for CandidateEdge {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for CandidateEdge {
    fn cmp(&self, other: &Self) -> Ordering {
        self.weight.cmp(&other.weight)
    }
}

impl Zero for CandidateEdge {
    fn zero() -> Self {
        CandidateEdge::default()
    }

    fn is_zero(&self) -> bool {
        self.weight.is_zero()
    }
}

impl Add<Self> for CandidateEdge {
    type Output = Self;

    #[inline]
    fn add(self, rhs: Self) -> Self::Output {
        CandidateEdge {
            weight: self.weight.saturating_add(rhs.weight),
        }
    }
}

impl CandidateEdge {
    #[inline]
    pub fn new(weight: u32) -> Self {
        Self { weight }
    }
}
