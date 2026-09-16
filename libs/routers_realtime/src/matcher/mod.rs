//! The regional matcher: load a graph, pull jobs, solve, publish results.
//!
//! A matcher replica serves one `(graph, region)`. It loads the pinned graph
//! before it ever touches the work queue, then runs a capacity-bounded pull loop
//! that never claims more work than it has slots to solve. It owns no vehicle
//! state and writes no authoritative output — it answers one [`SolveJob`](crate::protocol::SolveJob)
//! with one [`SolveResult`](crate::protocol::SolveResult) and lets the owning
//! orchestrator decide what to commit.
//!
//! The modules follow the spec's matcher decomposition (spec §4):
//!
//! * [`bootstrap`] — graph bootstrap: verify each artifact's checksum/version
//!   against the manifest, build the network, and report `Ready` only when it is
//!   usable.
//! * [`pull`] — the capacity-bound pull loop: fetch only as many jobs as there
//!   are free slots, manage acks/naks and bounded redelivery.
//! * [`validate`] — the job validator: envelope, decoded-size bound, expected
//!   graph, supported coverage, remaining deadline.
//! * [`engine`] — the matching engine: candidate generation, Viterbi solve,
//!   convergence, and the typed [`SolveOutcome`](crate::protocol::SolveOutcome)
//!   it returns.
//! * [`publish`] — the result publisher: publish the correlated result and await
//!   the broker's acknowledgement *before* acknowledging the job.
//!
//! Draining and readiness are handled by [`lifecycle`](crate::lifecycle): a
//! readiness flag never stops the pull loop on its own.

pub mod bootstrap;
pub mod engine;
pub mod publish;
pub mod pull;
pub mod validate;
