use crate::transition::Strategy;
use routers_network::edge::Weight;

pub trait EmissionStrategy: for<'a> Strategy<EmissionContext<'a>> {}
impl<T> EmissionStrategy for T where T: for<'a> Strategy<EmissionContext<'a>> {}

#[derive(Clone, Copy, Debug)]
pub struct EmissionContext<'a> {
    /// The proposed (candidate) position to be matched onto.
    ///
    /// This belongs to the network, and is not provided
    /// as input to the match query.
    pub candidate_position: &'a geo::Point,

    /// The position the costing method is matching.
    ///
    /// This belongs to the un-matched trip, as the position
    /// which must be matched upon the network.
    pub source_position: &'a geo::Point,

    /// The distance (in meters) between the source and candidate positions using [`geo::Haversine`].
    ///
    /// Note: This is given as it is used in the processing step, and if it were
    /// used during the costing stage it is more optimal to pass it on rather than
    /// calculate it twice.
    pub distance: f64,

    /// The road-class weight of the candidate edge (from [`RoadClass::weighting`]).
    ///
    /// Lower values indicate higher-priority roads (e.g. `Motorway = 1`).
    /// Passed through so that emission strategies can discount candidates on
    /// lower-quality roads relative to those on higher-quality roads at the
    /// same physical distance.
    pub weight: Weight,
}

impl<'a> EmissionContext<'a> {
    pub fn new(
        candidate: &'a geo::Point,
        source: &'a geo::Point,
        distance: f64,
        weight: Weight,
    ) -> Self {
        Self {
            candidate_position: candidate,
            source_position: source,
            distance,
            weight,
        }
    }
}
