use geo::Point;
use routers_trellis::{SolveError, TrellisError};
use thiserror::Error;

/// Why a map-match could not be produced.
///
/// Variants are split by the *stage* at which matching gave up, so a caller can
/// tell an unroutable trajectory apart from a lower-level failure — and, where
/// relevant, learn *which* part of the trajectory was at fault.
#[derive(Error, Debug)]
pub enum MatchError {
    /// One or more trajectory points could not be anchored to the road network:
    /// no candidate edge lay within the search radius.
    #[error(transparent)]
    Unanchored(#[from] UnanchoredError),

    /// Candidates were found for every point, but no continuous route links
    /// them: the trajectory breaks at one or more boundaries, where no candidate
    /// reachable up to that point can reach any candidate in the next layer.
    #[error(transparent)]
    Disconnected(#[from] DisconnectedError),

    /// The trellis rejected graph construction or a weight fill. An empty input
    /// trajectory surfaces here as [`TrellisError::Empty`].
    #[error("trellis error: {0}")]
    TrellisError(#[from] TrellisError),

    /// The path solver could not run over the (fully weighed) trellis.
    #[error("solver error: {0}")]
    SolveError(#[from] SolveError),
}

/// Every trajectory point that could not be placed on the road network,
/// collected into one error so the caller can address them together.
#[derive(Error, Debug, Clone, PartialEq)]
#[error("{} trajectory point(s) could not be anchored to the network", .points.len())]
pub struct UnanchoredError {
    /// The off-network points, in trajectory order.
    pub points: Vec<Unanchored>,
}

/// A trajectory point that could not be placed on the road network — no
/// candidate edge was found within the configured search radius.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Unanchored {
    /// Index of the point in the input trajectory (equivalently, its layer).
    pub layer: usize,

    /// The input coordinate that had no nearby candidate.
    pub origin: Point,
}

/// Every boundary at which the trajectory breaks, collected into one error so
/// the caller can address all gaps together.
#[derive(Error, Debug, Clone, PartialEq)]
#[error("route breaks at {} boundary/boundaries in the trajectory", .breaks.len())]
pub struct DisconnectedError {
    /// The broken boundaries, in layer order.
    pub breaks: Vec<Disconnected>,
}

/// The boundary between two adjacent layers across which the route breaks: no
/// candidate reachable up to `from_layer` can reach any candidate in `to_layer`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Disconnected {
    /// The layer on the near side of the broken boundary.
    pub from_layer: usize,

    /// The layer on the far side of the broken boundary (`from_layer + 1`).
    pub to_layer: usize,

    /// The input coordinate anchoring `from_layer`.
    pub from_origin: Point,

    /// The input coordinate anchoring `to_layer`.
    pub to_origin: Point,
}
