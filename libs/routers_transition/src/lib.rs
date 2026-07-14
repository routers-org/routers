//! Hidden-Markov-Model map-matching: anchoring raw positional data onto an
//! underlying road network.
//!
//! The lifecycle is a borrowed [`Matcher`] (configuration + operations)
//! driving a caller-owned [`Trip`] (all mutable state): candidates generate
//! per layer (emission costs become trellis node weights), a [`Weigher`]
//! fills pending boundary transitions, and `routers_trellis` finds the
//! minimum-cost path. Batch callers use [`Matcher::r#match`] (or the
//! [`Match`] facade); realtime callers loop `push` → `solve` and `finish`
//! when done.

extern crate alloc;

pub mod candidate;
pub mod costing;
pub mod layer;
pub mod map_path;
pub mod r#match;
pub mod matcher;
pub mod primitives;
pub mod weigh;

pub use r#match::DEFAULT_SEARCH_DISTANCE;
pub use r#match::Match;

// The trellis types appearing in this crate's public API (the caller owns the
// trip whose trellis is grown and solved), re-exported so consumers need not
// depend on `routers_trellis` directly. The trellis path is aliased so it
// cannot shadow the match facade's [`Path`](crate::candidate::route::Path).
pub use routers_trellis::{LayerId, NodeId, Path as TrellisPath, Solved, Trellis};

// Re-Exports
#[doc(inline)]
pub use candidate::*;
#[doc(inline)]
pub use costing::*;
#[doc(inline)]
pub use matcher::*;
#[doc(inline)]
pub use primitives::*;
#[doc(inline)]
pub use weigh::*;

pub use layer::*;
pub use map_path::*;
