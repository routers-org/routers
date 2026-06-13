//! Incremental (streaming) HMM matching primitives.
//!
//! See `PARTIAL_STATE_MATCHING.md` at the repo root for the design.
//!
//! Phase 1A (current): anchor-based 1-best warm step. The matcher
//! preserves the snapped position per vehicle and feeds it back as
//! `MatchOptions::anchor` on the next event, reducing the trellis from
//! a 6-point linestring (5 history + 1 current) to a 2-point linestring
//! (anchor + 1 current). Zero solver modifications, but no multi-
//! hypothesis frontier preservation — if the prior match was wrong on
//! an ambiguous intersection, we can't revise.
//!
//! Phase 1B (future): full Viterbi frontier preservation in a new
//! solver variant. See `PARTIAL_STATE_MATCHING.md` §Design for details.

pub mod state;

pub use state::MatchState;
