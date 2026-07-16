//! This crate finds the most plausible route a trajectory took through a network.
//!
//! This crate is modelled by a Hidden Markov Model, solved as a minimum-cost path
//! problem using a [`Trellis`](routers_trellis::Trellis) and a [`Network`](routers_network::Network).
//!
//! ## Getting Started
//!
//! Every [`Network`](routers_network::Network) implements the [`Match`](routers_transition::match::Match)
//! trait, so if you have a map loaded _there is nothing to construct_.
//!
//! ```ignore
//! use routers_transition::{Match, MatchOptions};
//!
//! // Simply, match a linestring against the network.
//! let routed = network.r#match(linestring, MatchOptions::default())?;
//! ```
//!
//! This returns a [`RoutedPath`]. A routed path is the simplest output for
//! a map match, and contains two key fields; [`discretized`](RoutedPath::discretized)
//! and [`interpolated`](RoutedPath::interpolated). These describe the 1:1
//! correspondence between input points and matched road positions and the
//! 1:N interpolated geometries respectively.
//!
//! If you want to dive a little deeper, you can tune the candidate search
//! radius, weighing strategy, and caching through [`MatchOptions`], instead
//! of supplying the defaults.
//!
//! ### Going deeper
//!
//! For everything beyond a basic configuration, use the [`Matcher`] struct directly.
//! By doing so, you can gain access to the lifetimes of it's inputs to optimise
//! caching, as well as the batch and streaming lifecycles.
//!

extern crate alloc;

#[doc(inline)]
pub use r#match::{MatchOptions, MatchSimpleExt};
#[doc(inline)]
pub use matcher::Matcher;
#[doc(inline)]
pub use primitives::MatchError;

#[doc(inline)]
pub use r#match::Match;

pub mod candidate;
pub mod costing;
pub mod layer;
pub mod matcher;
pub mod primitives;
pub mod weigh;

mod map_path;
mod r#match;

// Re-exports from routers_trellis
pub use routers_trellis::{LayerId, NodeId, Path as TrellisPath, Solved, Trellis};
