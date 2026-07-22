//! The match lifecycle: a borrowed [`Matcher`] (configuration and operations)
//! driving a caller-owned [`Trip`] (all mutable state).
//!
//! Keeping all state in the [`Trip`] is what makes streaming possible: a trip
//! is pure data — serializable, inspectable, and owned by you — so it can be
//! persisted between ticks, resumed in another process, and handed back to
//! any matcher configured the same way. Start at [`Matcher`] for the full
//! batch and streaming walkthroughs.

mod continuation;
mod entity;
mod trip;

pub use continuation::Continuation;
pub use entity::Matcher;
pub use trip::{Trip, TripState};
