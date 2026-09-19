//! Bus adapter traits: publication, consumption, and acknowledgement.
//!
//! A transport-only seam so the same calls drive the in-memory fake in
//! [`super::memory`] under test and JetStream in production. Futures are not
//! `Send` — callers own them on a single task, so native `async fn` in traits is fine.
#![allow(async_fn_in_trait)]

use core::time::Duration;

use async_nats::HeaderMap;
use thiserror::Error;
use web_time::SystemTime;

use crate::bus::Wire;

/// A handle to acknowledge exactly one delivered message; consuming `self` makes
/// double-acking a compile error.
pub trait AckHandle: Send + 'static {
    /// Positively acknowledge; the broker may retire the message.
    async fn ack(self) -> anyhow::Result<()>;

    /// Negatively acknowledge; `delay` is a redelivery backoff hint, `None` meaning soon.
    async fn nak(self, delay: Option<Duration>) -> anyhow::Result<()>;

    /// The message's stream sequence — its durable position.
    fn sequence(&self) -> u64;

    /// Delivery count including the current delivery (`1` on first delivery).
    fn deliveries(&self) -> u32;
}

/// One message taken off the bus together with the handle that acknowledges it.
pub struct Delivery<T, H: AckHandle> {
    pub item: T,
    pub handle: H,
    pub subject: String,
    /// The broker dedup key (`Nats-Msg-Id`), if one was set.
    pub msg_id: Option<String>,
    pub headers: HeaderMap,
    /// Durable broker publish time when available; test/in-memory adapters may
    /// derive it from the producer's stable send-time header.
    pub sent_at: Option<SystemTime>,
    pub redelivered: bool,
}

/// The broker's answer to a publish that reached it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The stream stored the message (or recognised it as a duplicate).
    Acked {
        /// The stored message's stream sequence; for a duplicate, the original's.
        sequence: u64,
        /// `true` when the `Nats-Msg-Id` dedup-matched, so no new copy was stored.
        duplicate: bool,
    },
}

/// Why a publish did not return a confirmed [`PublishOutcome`]. `Ambiguous` may
/// already have landed (retry idempotently); `Failed` is known not to have.
#[derive(Debug, Error)]
pub enum PublishError {
    /// The outcome is unknown; retry with the same bytes and `msg_id` to dedup.
    #[error("ambiguous publish (may or may not have landed): {0}")]
    Ambiguous(anyhow::Error),
    /// The publish is known to have failed; nothing was stored.
    #[error("publish failed: {0}")]
    Failed(anyhow::Error),
}

impl PublishError {
    /// Whether the publish was ambiguous — the caller must retry idempotently.
    #[must_use]
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, PublishError::Ambiguous(_))
    }
}

/// Publishes messages of one wire type to subjects on the bus.
pub trait Publisher<T: Wire>: Clone + Send + Sync + 'static {
    /// Publish pre-encoded `bytes` under `subject` with dedup key `msg_id`.
    async fn publish_bytes(
        &self,
        subject: &str,
        msg_id: &str,
        headers: HeaderMap,
        bytes: &[u8],
    ) -> Result<PublishOutcome, PublishError>;

    /// Encode `item` and publish it; an encoding failure is a [`PublishError::Failed`].
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

/// A continuous, push-style stream of deliveries. [`next`](Self::next) is
/// cancellation-safe: dropping the future before it resolves consumes no message.
pub trait Source<T: Wire>: Send {
    type Handle: AckHandle;

    /// Await the next delivery; `None` only once the bus is closed and drained.
    async fn next(&mut self) -> Option<anyhow::Result<Delivery<T, Self::Handle>>>;
}

/// A capacity-bounded pull consumer, where `max` bounds messages held in flight.
pub trait Consumer<T: Wire>: Send {
    type Handle: AckHandle;

    /// Pull up to `max` messages, waiting at most `wait`; the `Vec` may be
    /// shorter than `max`, or empty if nothing arrived in time.
    async fn fetch(
        &mut self,
        max: usize,
        wait: Duration,
    ) -> anyhow::Result<Vec<Delivery<T, Self::Handle>>>;
}
