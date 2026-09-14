//! The shared protocol: identities, versions, and the three control-plane
//! messages (solve job, solve result, committed output).

pub mod ids;
pub mod job;
pub mod output;
pub mod result;

pub use ids::*;
