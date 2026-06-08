use geo::Point;
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
