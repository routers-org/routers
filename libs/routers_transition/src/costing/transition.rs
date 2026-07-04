use crate::ResolutionMethod;
use crate::candidate::{Candidate, CandidateId, Candidates};
use crate::{RoutingContext, Strategy, Trip, VirtualTail};
use geo::{Distance, Haversine, Point};
use routers_network::{Entry, Metadata, Network};

pub trait TransitionStrategy<E>: for<'a> Strategy<TransitionContext<'a, E>> {}
impl<T, E> TransitionStrategy<E> for T where T: for<'a> Strategy<TransitionContext<'a, E>> {}

/// Edge-level bearings for the source and target candidate edges.
///
/// A field is `None` when [`Candidate::edge_heading`] could not resolve
/// a bearing where either the endpoint is missing from the map, or the edge
/// is degenerate (endpoints small distance apart).
#[derive(Clone, Copy, Debug)]
pub struct Headings {
    pub source: Option<f64>,
    pub target: Option<f64>,
}

#[derive(Clone, Copy, Debug)]
pub struct VirtualTails {
    /// [`VirtualTail::ToTarget`] on the source candidate:
    /// Effectively `distance(source_position, source.edge.target)`.
    pub source: Option<f64>,
    /// [`VirtualTail::ToSource`] on the target candidate:
    /// Effectively `distance(target.edge.source, target_position)`.
    pub target: Option<f64>,
}

#[derive(Clone, Debug)]
pub struct TransitionContext<'a, E>
where
    E: Entry + 'a,
{
    /// The optimal path travelled between the
    /// source candidate and target candidate, used
    /// to determine trip complexity (and therefore
    /// cost) often through heuristics such as
    /// immediate and summative angular rotation.
    ///
    /// Contains interior map nodes only — the candidate endpoint positions
    /// are tracked separately in [`source_position`](Self::source_position)
    /// and [`target_position`](Self::target_position) so that callers which
    /// only care about the routed geometry are not forced to special-case
    /// synthetic endpoints.
    pub optimal_path: Trip<E>,

    /// The source candidate's position on its edge. Used together with
    /// [`optimal_path`](Self::optimal_path) when evaluating intra-transition
    /// geometry (e.g. the turn induced at the candidate→edge-endpoint joint).
    pub source_position: Point,

    /// The target candidate's position on its edge. See [`source_position`](Self::source_position).
    pub target_position: Point,

    /// A list of all OSM nodes pertaining to the optimal trip path.
    pub map_path: &'a [E],

    /// The source candidate indicating the edge and
    /// position for which the path begins at.
    pub source_candidate: &'a CandidateId,

    /// The target candidate indicating the edge and
    /// position for which the path ends at.
    pub target_candidate: &'a CandidateId,

    /// Candidate registry used to resolve candidates into full [`Candidate`] values.
    pub candidates: &'a Candidates<E>,

    /// The requested [resolution method](ResolutionMethod) by which the transition costing function
    /// should attempt to cost (resolve) the two candidates. Defaults to
    /// [`ResolutionMethod::Standard`]; override with
    /// [`with_resolution_method`](Self::with_resolution_method).
    pub requested_resolution_method: ResolutionMethod,

    /// Edge bearings for source and target candidates.
    pub headings: Headings,

    /// Per-candidate virtual-tail distances.
    pub virtual_tails: VirtualTails,
}

pub struct TransitionLengths {
    /// The great circle distance between source and target
    pub straightline_distance: f64,

    /// The path of the optimal route between candidates
    pub route_length: f64,
}

impl TransitionLengths {
    /// Calculates the deviance in straightline distance to the length
    /// of the entire route. Returns values between 0 and 1. Where values
    /// closer to 1 represent more optimal distances, whilst those closer
    /// to 0 represent less optimal distances.
    ///
    /// The route length is defined as the cumulative distance between
    /// nodes in the optimal transition path, plus the offsets into the
    /// edges by which the candidates live.
    ///
    /// The straightline distance is defined as the haversine (great circle)
    /// distance between the two candidates.
    ///
    /// Therefore, our deviance is defined as the ratio of straightline
    /// distance to the route length, which measures how much farther
    /// the actual route was than a virtual path directly between the candidates.
    ///
    /// For example:
    /// -   If two candidates were `100m` apart, but had a most optimal route
    ///     between them of `130m`, the deviance would be `~0.77`.
    /// -   If two alternate candidates were `100m` apart but instead had an
    ///     optimal route between them of `250m`, the deviance is `0.4`.
    ///
    /// Note that a lower deviance score means the values are less aligned.
    #[inline]
    pub fn deviance(&self) -> f64 {
        if self.route_length <= 0.0 {
            return 1.0;
        }

        self.straightline_distance / self.route_length
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_deviance_zero_length() {
        let lengths = TransitionLengths {
            straightline_distance: 0.0,
            route_length: 0.0,
        };
        assert_eq!(lengths.deviance(), 1.0);
    }
}

impl<'a, E> TransitionContext<'a, E>
where
    E: Entry,
{
    /// Builds the context, precomputing all network-derived values so the
    /// resulting struct is generic only over the entry type.
    ///
    /// Candidate positions and the [`Trip`] representation of the optimal
    /// path are derived internally — the caller supplies the candidate IDs
    /// and the map-node sequence between them.
    pub fn new<M, N>(
        ctx: &'a RoutingContext<'a, E, M, N>,
        (src, trg): (&'a CandidateId, &'a CandidateId),
        map_path: &'a [E],
    ) -> Option<Self>
    where
        M: Metadata,
        N: Network<E, M>,
    {
        let source = ctx.candidate(src)?;
        let target = ctx.candidate(trg)?;

        let headings = Headings {
            source: source.edge_heading(ctx),
            target: target.edge_heading(ctx),
        };

        let virtual_tails = VirtualTails {
            source: source.offset(ctx, VirtualTail::ToTarget),
            target: target.offset(ctx, VirtualTail::ToSource),
        };

        Some(Self {
            optimal_path: Trip::new_with_map(ctx.map, map_path),
            candidates: ctx.candidates,
            requested_resolution_method: ResolutionMethod::default(),

            source_position: source.position,
            target_position: target.position,

            source_candidate: src,
            target_candidate: trg,

            map_path,
            headings,
            virtual_tails,
        })
    }

    /// Overrides the [resolution method](ResolutionMethod) used when costing
    /// the transition.
    pub fn with_resolution_method(mut self, method: ResolutionMethod) -> Self {
        self.requested_resolution_method = method;
        self
    }

    /// Obtains the source [candidate](Candidate) from the context.
    pub fn source_candidate(&self) -> Candidate<E> {
        self.candidates
            .candidate(self.source_candidate)
            .expect("source candidate not found")
    }

    /// Obtains the target [candidate](Candidate) from the context.
    pub fn target_candidate(&self) -> Candidate<E> {
        self.candidates
            .candidate(self.target_candidate)
            .expect("target candidate not found")
    }

    /// Returns a [candidate](Candidate) pair of (source, target)
    pub fn candidates(&self) -> (Candidate<E>, Candidate<E>) {
        (self.source_candidate(), self.target_candidate())
    }

    pub fn angular_complexity(&self) -> f64 {
        self.optimal_path.angular_complexity_with_headings(
            self.headings.source,
            self.headings.target,
            self.source_position,
            self.target_position,
        )
    }

    /// Calculates the total offset, of both source and target positions within the context,
    /// considering the resolution method requested.
    pub fn total_offset(&self, source: &Candidate<E>, target: &Candidate<E>) -> Option<f64> {
        match self.requested_resolution_method {
            ResolutionMethod::Standard => {
                let inner_offset = self.virtual_tails.source?;
                let outer_offset = self.virtual_tails.target?;
                Some(inner_offset + outer_offset)
            }
            ResolutionMethod::DistanceOnly => {
                Some(Haversine.distance(source.position, target.position))
            }
        }
    }

    /// Returns the [`TransitionLengths`] of the context.
    pub fn lengths(&self) -> Option<TransitionLengths> {
        let (source, target) = self.candidates();
        let offset = self.total_offset(&source, &target)?;

        let route_length = self.optimal_path.length() + offset;
        let straightline_distance = Haversine.distance(source.position, target.position);

        Some(TransitionLengths {
            straightline_distance,
            route_length,
        })
    }
}
