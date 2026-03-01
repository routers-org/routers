//! A Hidden-Markov-Model (HMM) matching
//! transition module that allows for
//! matching raw data to an underlying
//! network.

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
