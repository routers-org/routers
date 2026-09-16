//! The message bus: how bytes cross NATS, and the seams that make it testable.
//!
//! Everything the pipeline sends is framed here before it reaches a broker.
//! [`Wire`] is the framing contract — Rust-internal control-plane messages
//! encode as postcard through the `postcard_wire!` macro, boundary messages as
//! protobuf — so the [`topology`](crate::topology) names and the payloads that
//! ride under them stay separable.
//!
//! The delivery mechanics sit behind adapter traits so no business logic ever
//! touches a live connection in a test:
//!
//! * [`adapter`] — the [`Publisher`](adapter::Publisher)/[`Source`](adapter::Source)/[`AckHandle`](adapter::AckHandle)
//!   traits: publish-with-acknowledgement, pull-with-ack, and the ambiguous-vs-failed
//!   publish outcome the dispatcher retries on.
//! * [`memory`] — [`MemoryBus`](memory::MemoryBus) and its fakes: an in-process
//!   broker with msg-id dedup, ack/nak bookkeeping, and crash knobs, since the
//!   sandbox has no NATS.
//! * [`jetstream`] — the production adapters that bind those traits to
//!   `async_nats` JetStream consumers and publishers.
//! * `trace` — the wall-clock trace headers ([`outbound`], [`inbound`]) that
//!   let a span follow a message across planes.
//! * [`NATSStream`] — a thin `Stream` over a core-NATS subscriber, decoding each
//!   frame back through [`Wire`].

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
/// [`postcard_wire!`]; boundary messages the wider world produces or
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
