//! The partition orchestrator: vehicle continuity, admission, commit, recovery.
//!
//! One [`worker`] task owns each partition outright — its vehicle table,
//! scheduler, frontier tracker, and deadline heap — with no cross-partition
//! sharing, driving each vehicle read → schedule → admit → dispatch → validate →
//! commit → advance frontier. The deadline handler and the result validator
//! arbitrate the same per-vehicle transition, and a prepared commit always wins
//! over a later timeout. Draining is handled by [`lifecycle`](crate::lifecycle).

pub mod admission;
pub mod commit;
pub mod deadline;
pub mod dispatch;
pub mod frontier;
pub mod reader;
pub mod recovery;
pub mod scheduler;
pub mod validate;
pub mod worker;
