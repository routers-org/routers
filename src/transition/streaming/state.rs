//! Per-vehicle state carried between events by the streaming matcher.
//!
//! The road network is immutable within a pod's lifetime, so any state
//! derivable from the network is recomputed on demand. The only thing
//! that cannot be recomputed is the Viterbi cumulative cost accrued
//! across past events. That, plus the snapped position and stable
//! graph edge id, is what each frontier node carries.

use geo::Point;
use routers_network::{Edge, Entry};
use serde::{Deserialize, Serialize};

/// One candidate node in a saved Viterbi column.
///
/// Identifiers are stable across solver runs — `Edge<E>` is the
/// underlying graph edge, not a per-solve positional candidate id.
/// Saved frontiers therefore survive solver reconstructions and any
/// code path that does not modify the road network itself.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FrontierNode<E: Entry> {
    /// Underlying graph edge this candidate sits on.
    pub edge: Edge<E>,

    /// Snapped position on the edge — projection of the GPS
    /// observation onto the edge geometry.
    pub snapped: Point,

    /// Cumulative best-path Viterbi cost ending at this candidate.
    /// Seeds the next event's recurrence:
    /// `next.cum_cost = min_prev (prev.cum_cost + transition + emission)`.
    pub cum_cost: u32,
}

/// Per-vehicle cache entry for the streaming matcher.
///
/// The frontier is the saved transition-graph state. There is no
/// separate graph structure to persist — the road network is
/// immutable for the lifetime of a matcher instance.
///
/// # Invariants
///
/// - An empty `frontier` means no valid hypothesis carries forward;
///   the next event must cold-start. Use [`Self::is_empty`] to check
///   before attempting to extend.
/// - `last_event_ms` is the `resolved_at_ms` of the orchestrator
///   event that produced this frontier.
///
/// # Append
///
/// The streaming step extends a `MatchState` by one layer:
///
/// 1. Treat the current `frontier` as L0; each node's `cum_cost`
///    seeds the start → L0 emission.
/// 2. Generate L1 candidates for the new GPS point.
/// 3. Run the solver over the 2-layer trellis.
/// 4. Extract the L1 cumulative costs via a forward Viterbi sweep.
/// 5. Replace `frontier` with the L1 column.
///
/// By the Markov property this is identical to a full N+1-layer
/// solve over the same observations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MatchState<E: Entry> {
    /// Viterbi column at the most-recent matched layer.
    pub frontier: Vec<FrontierNode<E>>,

    /// `resolved_at_ms` of the event that produced this frontier.
    /// Used for last-writer-by-timestamp guards and TTL eviction.
    pub last_event_ms: u64,
}

impl<E: Entry> MatchState<E> {
    /// Construct a state from a frontier and event timestamp.
    pub fn new(frontier: Vec<FrontierNode<E>>, last_event_ms: u64) -> Self {
        Self {
            frontier,
            last_event_ms,
        }
    }

    /// Frontier node with the lowest `cum_cost` — the current best
    /// hypothesis for where the vehicle is.
    pub fn argmin(&self) -> Option<&FrontierNode<E>> {
        self.frontier.iter().min_by_key(|n| n.cum_cost)
    }

    /// Snapped position of the current best hypothesis.
    pub fn last_matched(&self) -> Option<Point> {
        self.argmin().map(|n| n.snapped)
    }

    /// Cumulative cost of the current best hypothesis.
    pub fn last_cum_cost(&self) -> Option<u32> {
        self.argmin().map(|n| n.cum_cost)
    }

    /// `true` if there is no hypothesis to extend; callers should
    /// cold-start the next event.
    pub fn is_empty(&self) -> bool {
        self.frontier.is_empty()
    }

    /// Number of hypotheses currently tracked.
    pub fn len(&self) -> usize {
        self.frontier.len()
    }

    /// Truncate the frontier to the top-K candidates by `cum_cost`.
    /// No-op if `k >= self.len()`.
    pub fn truncate_to_top(&mut self, k: usize) {
        if self.frontier.len() <= k {
            return;
        }
        self.frontier.sort_unstable_by_key(|n| n.cum_cost);
        self.frontier.truncate(k);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::MockEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};

    fn node(cum_cost: u32) -> FrontierNode<MockEntryId> {
        FrontierNode {
            edge: Edge {
                source: MockEntryId(0),
                target: MockEntryId(0),
                weight: 0,
                id: DirectionAwareEdgeId::new(MockEntryId(0)),
            },
            snapped: Point::new(0.0, 0.0),
            cum_cost,
        }
    }

    #[test]
    fn truncate_keeps_lowest_cost_k() {
        let mut state =
            MatchState::new(vec![node(30), node(10), node(20), node(40)], 0);
        state.truncate_to_top(2);
        let costs: Vec<u32> = state.frontier.iter().map(|n| n.cum_cost).collect();
        assert_eq!(costs, vec![10, 20]);
    }

    #[test]
    fn truncate_is_noop_when_below_cap() {
        let mut state = MatchState::new(vec![node(1), node(2)], 0);
        state.truncate_to_top(5);
        assert_eq!(state.len(), 2);
    }

    #[test]
    fn argmin_picks_minimum_cum_cost() {
        let state = MatchState::new(vec![node(100), node(5), node(50)], 0);
        assert_eq!(state.argmin().map(|n| n.cum_cost), Some(5));
    }

    #[test]
    fn is_empty_distinguishes_no_frontier() {
        let empty = MatchState::new(Vec::<FrontierNode<MockEntryId>>::new(), 0);
        assert!(empty.is_empty());
        let one = MatchState::new(vec![node(0)], 0);
        assert!(!one.is_empty());
    }
}
