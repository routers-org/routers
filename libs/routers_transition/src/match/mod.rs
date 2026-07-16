//! The one-call facade: match a trajectory against any network, without
//! assembling a [`Matcher`](crate::Matcher) yourself.
//!
//! [`Match`] is implemented for every [`Network`](routers_network::Network),
//! so matching is a single method call configured by [`MatchOptions`]. When
//! the defaults are all you need, [`MatchSimpleExt`] drops the options
//! argument too. Reach for the [`Matcher`](crate::Matcher) instead when you
//! need streaming, state persistence, or custom strategies.

mod definition;
mod implementation;

pub use definition::{DEFAULT_SEARCH_DISTANCE, Match, MatchOptions, MatchSimpleExt};
