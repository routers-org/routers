//! Real-time map matching over NATS JetStream.
//!
//! `routers_realtime` turns a stream of raw vehicle observations into a durable,
//! revisable history of matched output. Four roles pass messages over four
//! JetStream planes and never call each other directly — the bus is the only
//! coupling: [`ingress`] publishes observations, an [`orchestrator`] owns each
//! vehicle's continuity and commits output, a [`matcher`] solves jobs
//! statelessly, and the [`materializer`] applies committed output to the served
//! view. The `README.md` beside this file is the map from spec component to
//! code; this page is the quick index into it.
//!
//! # Where things live
//!
//! * [`protocol`] — the three control-plane messages ([`SolveJob`](protocol::SolveJob),
//!   [`SolveResult`](protocol::SolveResult), [`CommittedOutput`](protocol::CommittedOutput))
//!   and their deterministic identities.
//! * [`topology`] — the four planes' subject/stream/consumer names (wire law).
//! * [`bus`] — the [`Wire`](bus::Wire) framing and the publish/pull adapter seams,
//!   with in-memory fakes for tests.
//! * [`store`] — Valkey checkpoints, prepared commits, and frontiers, and the
//!   crash-safe prepare → publish → promote sequence.
//! * [`orchestrator`] — per-partition vehicle continuity, admission, commit,
//!   recovery (spec §3).
//! * [`matcher`] — graph bootstrap and the capacity-bound solve loop (spec §4).
//! * [`materializer`] — the idempotent served-view consumer.
//! * [`region`] — the catalog, resolver, and graph-artifact manifests.
//! * [`ingress`], [`lifecycle`], [`metrics`], [`telemetry`], [`partition`],
//!   [`event`] — the ingest surface, drain coordination, observability, the
//!   partition law, and the shared event types.
//!
//! No broker or store exists in the test sandbox: everything that talks to NATS
//! or Valkey sits behind an adapter trait and is exercised by in-memory fakes.

extern crate alloc;

pub mod bus;
pub mod event;
pub mod ingress;
pub mod lifecycle;
pub mod matcher;
pub mod materializer;
pub mod metrics;
pub mod orchestrator;
pub mod partition;
pub mod protocol;
pub mod region;
pub mod store;
pub mod telemetry;
pub mod topology;
