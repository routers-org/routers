//! Per-vehicle state carried between events by the streaming matcher.

use geo::Point;
use serde::{Deserialize, Serialize};

/// Per-vehicle cache entry for streaming match.
///
/// **Phase 1B (current)**: 1-best anchored warm step with Viterbi
/// cum_cost tracking. The matcher feeds `last_matched` as the next
/// event's `MatchOptions::anchor` and adds each new event's solve cost
/// to `last_cum_cost`. Allows cost-divergence detection without
/// requiring a full multi-candidate frontier.
///
/// **Phase 1C (future)**: extend `last_cum_cost` to a
/// `Vec<FrontierNode>` so the warm step picks an argmin over multiple
/// prior hypotheses instead of committing to the most-recent argmin.
/// `MatchState`'s public shape is forwards-compatible: 1B-clients see
/// the single-best snapped coord, 1C-clients will see the full
/// frontier.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchState {
    /// Most-recent matched (snapped) coord — used as the warm-step
    /// anchor on the next event.
    pub last_matched: Point,
    /// GPS timestamp of the event that produced this match. Used for
    /// out-of-order rejection and TTL eviction.
    pub last_event_ms: u64,
    /// Cumulative Viterbi cost along the best 1-best path from the
    /// vehicle's most-recent cold-start through to the latest event.
    /// Reset to the current event's solve cost on every cold-start.
    /// Incremented by the current event's solve cost on every warm
    /// step. Used for divergence detection — if it blows past a
    /// threshold, the warm-state is discarded and the next event
    /// cold-starts to re-anchor.
    pub last_cum_cost: u32,
}

impl MatchState {
    pub fn new(last_matched: Point, last_event_ms: u64, last_cum_cost: u32) -> Self {
        Self {
            last_matched,
            last_event_ms,
            last_cum_cost,
        }
    }
}
