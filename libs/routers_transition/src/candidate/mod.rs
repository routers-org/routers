//! The candidate data model: the road positions a trajectory point may anchor
//! to, and the results a match is expressed in.
//!
//! A [`Candidate`] is one possible anchoring of one input point — an edge, a
//! position along it, and the emission cost of choosing it. Candidates are
//! stored per layer in a [`CandidateStore`] and identified positionally by a
//! [`CandidateRef`] (layer, node): identity *is* placement, so a ref is enough
//! to find its candidate in O(1).
//!
//! Results build upwards from there. A [`CollapsedPath`] is the matcher-level
//! result: the chosen candidate per layer plus the routed hops between them.
//! A [`RoutedPath`] is the facade-level result: the same information resolved
//! against the network into render-ready, metadata-carrying [`Path`]s.

mod collapse;
mod entry;
mod ident;
mod route;
mod store;

#[doc(inline)]
pub use collapse::CollapsedPath;
#[doc(inline)]
pub use route::{Path, PathElement, RoutedPath};

pub use entry::{Candidate, VirtualTail};
pub use ident::CandidateRef;
pub use store::CandidateStore;
