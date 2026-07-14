//! Candidates: the road positions a trajectory point may anchor to, stored
//! per layer and identified positionally by [`CandidateRef`].

pub mod collapse;
pub mod entry;
pub mod ident;
pub mod route;
pub mod store;

#[doc(inline)]
pub use collapse::*;
#[doc(inline)]
pub use entry::*;
#[doc(inline)]
pub use ident::*;
#[doc(inline)]
pub use route::*;
#[doc(inline)]
pub use store::*;
