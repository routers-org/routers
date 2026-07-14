//! The match lifecycle: a borrowed [`Matcher`] (configuration + operations)
//! over a caller-owned [`Trip`] (all mutable state).

mod entity;
mod trip;

#[doc(inline)]
pub use entity::*;
#[doc(inline)]
pub use trip::Trip;
