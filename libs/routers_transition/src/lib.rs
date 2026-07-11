//! A Hidden-Markov-Model (HMM) matching
//! transition module that allows for
//! matching raw data to an underlying
//! network.
//!

extern crate alloc;

pub mod candidate;
pub mod costing;
pub mod entity;
pub mod layer;
pub mod r#match;
pub mod primitives;
pub mod solver;
pub mod trip;

pub use r#match::DEFAULT_SEARCH_DISTANCE;
pub use r#match::Match;

// The trellis types appearing in this crate's public API (the caller owns the
// trellis a solver fills), re-exported so consumers need not depend on
// `routers_trellis` directly.
pub use routers_trellis::{LayerId, NodeId, Path, Trellis};

// Re-Exports
#[doc(inline)]
pub use candidate::*;
#[doc(inline)]
pub use costing::*;
#[doc(inline)]
pub use primitives::*;
#[doc(inline)]
pub use solver::*;

pub use entity::*;
pub use layer::*;
pub use trip::*;
