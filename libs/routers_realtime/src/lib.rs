//! Real-time map matching over NATS JetStream.
//!
//! `routers_realtime` turns a stream of raw vehicle observations into a durable,
//! revisable history of matched output. Four roles pass messages over four
//! JetStream planes and never call each other directly — the bus is the only
//! coupling: [`ingress`] publishes observations, an [`orchestrator`] owns each
//! vehicle's continuity and commits output, a [`matcher`] solves jobs
//! statelessly, and the [`materializer`] applies committed output to the served
//! view. See `README.md` for the map from spec component to code.
//!
//! Everything that talks to NATS or Valkey sits behind an adapter trait and is
//! exercised by in-memory fakes; no broker or store exists in the test sandbox.

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
