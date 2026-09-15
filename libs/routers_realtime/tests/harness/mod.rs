//! The harness crate's shared fixture and the scenario suites.
//!
//! Spec §6 ("Load/failure harness") asks for geographic skew, duplicates, crash
//! boundaries, checkpoint loss, sampling jitter, deadlines and scale-to-ready
//! delay; the §10 failure-responsibility rows the orchestrator and materialiser
//! own each get a scenario. They are grouped into four suites by theme so each
//! file stays readable:
//!
//! * [`scenarios_pipeline`] — the happy path, coalescing, idempotency, parking.
//! * [`scenarios_recovery`] — crash boundaries and graceful shutdown.
//! * [`scenarios_segments`] — segment continuity: state loss, teleport,
//!   regression suppression, unserved coverage.
//! * [`scenarios_scheduling`] — deadlines, the deadline/commit race, contiguous
//!   frontier advance, and admission backpressure.

pub mod fixture;

mod scenarios_pipeline;
mod scenarios_recovery;
mod scenarios_scheduling;
mod scenarios_segments;
