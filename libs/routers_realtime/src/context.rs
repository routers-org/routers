use geo::{Coord, Point};
use routers_shard::ShardId;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct RawEvent {
    pub vehicle_id: String,
    pub coord: Point,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub coord: Point,
    pub timestamp_ms: u64,
    /// Wall-clock ms assigned by the orchestrator when it processed this event.
    /// Used as a stable CRDT key: (vehicle_id, resolved_at_ms) uniquely identifies
    /// a processing event and allows the matcher to emit corrections when it
    /// re-evaluates the same history point with more context.
    pub resolved_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(deserialize = "S: serde::de::DeserializeOwned"))]
pub struct MatchContext<S: ShardId> {
    pub vehicle_id: String,
    pub resolved_at_ms: u64,
    pub history: Vec<Position>,
    pub current: Position,
    pub target_shard: S,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchResult {
    pub vehicle_id: String,
    /// Copied from MatchContext — when the orchestrator processed the raw event.
    pub resolved_at_ms: u64,
    /// Wall-clock ms when the matcher finished. resolved_at_ms − matched_at_ms = match latency.
    pub matched_at_ms: u64,
    pub coord: Point,
    pub outcome: MatchOutcome,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MatchOutcome {
    Success,
    /// HMM returned no candidates or the path was empty; coord is the raw GPS position.
    NoCandidate,
    /// The matching algorithm returned an error.
    Error,
}

/// The interpolated road geometry for a matched window.
///
/// Published to `matched.routes.{vehicle_id}` after each successful HMM solve.
/// `resolved_at_ms` is the orchestrator timestamp of the newest (current) event
/// in the window, tying this route to the same CRDT key as the trailing MatchResult.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatchRoute {
    pub vehicle_id: String,
    pub resolved_at_ms: u64,
    pub polyline: Vec<Coord>,
}
