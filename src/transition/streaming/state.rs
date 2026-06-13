//! Per-vehicle state carried between events by the streaming matcher.

use geo::Point;
use serde::{Deserialize, Serialize};

/// Phase 1A: minimal per-vehicle cache entry. Just the most-recent
/// matched position + timestamp. The matcher feeds `last_matched` back
/// as the next event's `MatchOptions::anchor`.
///
/// Phase 1B will extend this to carry the full Viterbi frontier
/// (`Vec<FrontierNode<E>>`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchState {
    /// Most-recent matched (snapped) coord.
    pub last_matched: Point,
    /// GPS timestamp of the event that produced this match. Used for
    /// out-of-order rejection and TTL eviction.
    pub last_event_ms: u64,
}

impl MatchState {
    pub fn new(last_matched: Point, last_event_ms: u64) -> Self {
        Self {
            last_matched,
            last_event_ms,
        }
    }
}
