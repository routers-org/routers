//! The partition orchestrator: vehicle continuity, admission, commit, recovery.
//!
//! One [`worker`] task owns each partition outright — its vehicle table,
//! scheduler and frontier tracker — with no cross-partition sharing, driving
//! each vehicle read → schedule → admit → dispatch → validate → commit →
//! advance frontier. Raw age is an observed freshness SLA, not a terminal
//! correctness condition. Draining is handled by [`lifecycle`](crate::lifecycle).

pub mod admission;
pub mod commit;
pub mod dispatch;
pub mod frontier;
pub mod reader;
pub mod recovery;
pub mod scheduler;
pub mod sharded;
pub mod validate;
pub mod worker;
