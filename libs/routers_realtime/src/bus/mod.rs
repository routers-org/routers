//! The message bus: how framed bytes cross NATS.
//!
//! [`Wire`] frames everything before it reaches a broker — control-plane
//! messages as postcard, boundary messages as protobuf — so the
//! [`topology`](crate::topology) names and their payloads stay separable.
//! Delivery sits behind the [`adapter`] traits so no business logic touches a
//! live connection in a test.

pub mod adapter;
pub mod jetstream;
pub mod memory;
mod nats;
mod trace;

pub use nats::NATSStream;
pub use trace::{inbound, outbound, span_between, wallclock};

/// How a message crosses the bus.
///
/// Rust-internal messages (the match control plane) use postcard via
/// `postcard_wire!`; boundary messages the wider world produces or
/// consumes (the raw ingest surface) encode as protobuf against the
/// `routers.realtime.v1` schema, so any language can speak them.
pub trait Wire: Sized {
    fn encode(&self) -> anyhow::Result<Vec<u8>>;
    fn decode(bytes: &[u8]) -> anyhow::Result<Self>;
}

/// Carry a serde message over the bus as postcard.
macro_rules! postcard_wire {
    ($name:ident $(< $($p:ident : $bound:path),+ >)?) => {
        impl $(< $($p: $bound + serde::Serialize + serde::de::DeserializeOwned),+ >)?
            $crate::bus::Wire for $name $(< $($p),+ >)?
        {
            fn encode(&self) -> anyhow::Result<Vec<u8>> {
                Ok(postcard::to_allocvec(self)?)
            }

            fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
                Ok(postcard::from_bytes(bytes)?)
            }
        }
    };
}
pub(crate) use postcard_wire;
