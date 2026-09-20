//! The shared protocol: identities, versions, and the three control-plane
//! messages (solve job, solve result, committed output).

pub mod ids;
pub mod job;
pub mod output;
pub mod result;

pub use ids::*;
pub use job::{BaseState, JobIdentity, JobProof, SolveJob};
pub use output::{CommittedOutput, OutputKind, ResetReason, TerminalReason};
pub use result::{SolveOutcome, SolveResult};
