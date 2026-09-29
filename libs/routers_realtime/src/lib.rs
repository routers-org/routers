//! Real-time map matching over NATS and Valkey.
//!
//! `routers_realtime` turns a stream of raw vehicle observations into a durable,
//! revisable history of matched output. [`ingress`] publishes observations to
//! a durable raw journal; an [`orchestrator`] owns each vehicle's continuity,
//! sends transient requests to a stateless [`matcher`], and commits durable
//! output. The [`materializer`] applies committed output to the served view.
//!
//! Bus and checkpoint adapters have in-memory implementations for tests.

extern crate alloc;

pub mod bus;
pub mod event;
pub mod health;
pub mod ingress;
pub mod lifecycle;
pub mod matcher;
pub mod materializer;
pub mod metrics;
pub mod orchestrator;
pub mod partition;
pub mod protocol;
pub mod region;
pub mod secret;
pub mod store;
pub mod telemetry;
pub mod topology;
