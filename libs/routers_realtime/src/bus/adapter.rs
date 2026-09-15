//! Bus adapter traits: publication, consumption, and acknowledgement. (T12)
//!
//! These traits are the seam between the control-plane logic (orchestrator,
//! matcher, materializer) and the durable transport underneath it. They speak
//! only about *delivery mechanics* — publish these bytes, hand me the next
//! message, (n)ack this one — and hold none of the business state that decides
//! *what* to publish or *how* to react. That split is what lets every unit test
//! drive the real logic against the in-memory fake in [`super::memory`] while
//! production wires the same calls to JetStream (T32/T35), with nothing but the
//! adapter changing between the two.
//!
//! The traits use native `async fn` (Rust 2024): the crate deliberately avoids
//! `async-trait` boxing. The futures are not declared `Send`; the callers own
//! their futures on a single task and never require them to cross threads, so
//! the [`async_fn_in_trait`] discouragement does not apply here.
#![allow(async_fn_in_trait)]

use core::time::Duration;

use async_nats::HeaderMap;
use thiserror::Error;
use web_time::SystemTime;

use crate::bus::Wire;

/// A handle to acknowledge exactly one delivered message.
///
/// It is a trait rather than a concrete type so the acknowledgement travels
/// with the message it belongs to (JetStream needs the original reply subject;
/// the fake needs the consumer cursor), yet the code that decides *whether* to
/// ack does not care which transport produced it. Consuming `self` on
/// [`AckHandle::ack`]/[`AckHandle::nak`] makes double-acking a compile error.
pub trait AckHandle: Send + 'static {
    /// Positively acknowledge: the broker may retire the message and never
    /// redeliver it. Called only once the work it drove is durably recorded.
    async fn ack(self) -> anyhow::Result<()>;

    /// Negatively acknowledge: ask the broker to redeliver later. `delay` is a
    /// hint for how long to hold it back (a backoff); `None` means "as soon as
    /// convenient".
    async fn nak(self, delay: Option<Duration>) -> anyhow::Result<()>;

    /// The message's stream sequence — its durable position, used to advance a
    /// partition frontier or to correlate a result with its raw input.
    fn sequence(&self) -> u64;

    /// How many times the broker has delivered this message, counting the
    /// current delivery (`1` on first delivery, `2` after one redelivery). A
    /// climbing count is how a consumer notices a poison message.
    fn deliveries(&self) -> u32;
}

/// One message taken off the bus together with the handle that acknowledges it.
///
/// `item` is already decoded; `subject`, `msg_id`, `headers`, and `sent_at` are
/// the envelope facts a receiver validates against (the subject encodes the
/// partition, `msg_id` is the broker's dedup key, `headers` carry the schema and
/// trace context, `sent_at` feeds queue-wait timing). `redelivered` is `true`
/// when this is not the first delivery, so a handler can be defensive about work
/// it may have already half-done.
pub struct Delivery<T, H: AckHandle> {
    /// The decoded message body.
    pub item: T,
    /// The acknowledgement handle for this delivery.
    pub handle: H,
    /// The concrete subject the message was published to.
    pub subject: String,
    /// The broker dedup key (`Nats-Msg-Id`), if one was set.
    pub msg_id: Option<String>,
    /// The message's headers as published. The reader validates the schema
    /// header (`x-routers-schema`) out of these; an empty map means the producer
    /// stamped none. Carried whole rather than pre-parsed so the raw plane can
    /// hand bytes and headers together to a poison-aware reader.
    pub headers: HeaderMap,
    /// When the producer published it, if it stamped a send time.
    pub sent_at: Option<SystemTime>,
    /// `true` when the broker has delivered this message before.
    pub redelivered: bool,
}

/// The broker's answer to a publish that reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The stream stored the message (or recognised it as a duplicate).
    Acked {
        /// The stored message's stream sequence. For a duplicate this is the
        /// sequence of the *original* message the broker kept.
        sequence: u64,
        /// `true` when the `Nats-Msg-Id` matched one already inside the stream's
        /// dedup window, so no new copy was stored.
        duplicate: bool,
    },
}

/// Why a publish did not return a confirmed [`PublishOutcome`].
///
/// The distinction is the whole point: an [`PublishError::Ambiguous`] publish
/// (typically a timeout) *may already have landed*, so the safe response is to
/// retry with the identical `msg_id` and let the broker dedup it. A
/// [`PublishError::Failed`] publish is known not to have landed and can be
/// retried freely or surfaced as an error.
#[derive(Debug, Error)]
pub enum PublishError {
    /// The outcome is unknown; the message may or may not be stored. Retry with
    /// the same bytes and `msg_id` so a landed copy dedups.
    #[error("ambiguous publish (may or may not have landed): {0}")]
    Ambiguous(anyhow::Error),
    /// The publish is known to have failed; nothing was stored.
    #[error("publish failed: {0}")]
    Failed(anyhow::Error),
}

impl PublishError {
    /// Whether the publish was ambiguous — the caller must assume a copy may be
    /// stored and retry idempotently rather than treat it as a clean failure.
    #[must_use]
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, PublishError::Ambiguous(_))
    }
}

/// Publishes messages of one wire type to subjects on the bus.
///
/// [`Publisher::publish`] is the ordinary path — encode a value and send it —
/// and is a provided method over [`Publisher::publish_bytes`], the byte path
/// the commit coordinator uses to *re-publish the exact stored bytes* of a
/// prepared output so a retried commit is byte-identical and dedups.
pub trait Publisher<T: Wire>: Clone + Send + Sync + 'static {
    /// Publish pre-encoded bytes under `subject` with dedup key `msg_id` and the
    /// given `headers`. The one primitive every publish is built on.
    async fn publish_bytes(
        &self,
        subject: &str,
        msg_id: &str,
        headers: HeaderMap,
        bytes: &[u8],
    ) -> Result<PublishOutcome, PublishError>;

    /// Encode `item` and publish it. An encoding failure is a
    /// [`PublishError::Failed`] — nothing left this process, so a clean retry is
    /// safe.
    async fn publish(
        &self,
        subject: &str,
        msg_id: &str,
        headers: HeaderMap,
        item: &T,
    ) -> Result<PublishOutcome, PublishError> {
        let bytes = item.encode().map_err(PublishError::Failed)?;
        self.publish_bytes(subject, msg_id, headers, &bytes).await
    }
}

/// A continuous, push-style stream of deliveries — the shape a per-partition
/// reader loop wants (`select!` over the next message).
///
/// [`Source::next`] must be cancellation-safe: dropping the returned future
/// before it resolves loses nothing — no message is consumed until the future
/// yields one — so it is sound to race `next()` against a shutdown signal in a
/// `select!`.
pub trait Source<T: Wire>: Send {
    /// The acknowledgement handle this source hands out.
    type Handle: AckHandle;

    /// Await the next delivery. Resolves to `None` only once the bus is closed
    /// and drained; otherwise it waits (without busy-looping) for a message.
    async fn next(&mut self) -> Option<anyhow::Result<Delivery<T, Self::Handle>>>;
}

/// A capacity-bounded pull consumer — the shape the matcher wants, where `max`
/// is the number of free solve slots so it never pulls more work than it can
/// hold in flight.
pub trait Consumer<T: Wire>: Send {
    /// The acknowledgement handle this consumer hands out.
    type Handle: AckHandle;

    /// Pull up to `max` messages, waiting at most `wait` for the batch to fill.
    /// Returns as soon as at least one message is available, `max` are gathered,
    /// or `wait` elapses — so the returned `Vec` may be shorter than `max`, or
    /// empty if nothing arrived in time.
    async fn fetch(
        &mut self,
        max: usize,
        wait: Duration,
    ) -> anyhow::Result<Vec<Delivery<T, Self::Handle>>>;
}
