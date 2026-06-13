//! Incremental (streaming) HMM matching.
//!
//! Each event extends a saved Viterbi frontier by one layer instead
//! of re-solving the trellis from scratch. The forward Viterbi sweep
//! over a saved frontier is mathematically equivalent to a full
//! rebuild over the same input points: the Markov property of the
//! recurrence guarantees prefix-optimality.

pub mod matcher;
pub mod state;
pub mod viterbi;

pub use matcher::StreamingMatcher;
pub use state::{FrontierNode, MatchState};
pub use viterbi::ViterbiFrontier;
