//! JetStream implementations of the bus adapters. (T32, T35)
//!
//! These are the production wiring of the transport-only traits in
//! [`super::adapter`]: they turn `async_nats::jetstream` primitives into the
//! same `publish`/`fetch`/`next`/`ack` vocabulary the control-plane logic
//! already drives against the in-memory fake in [`super::memory`]. Nothing here
//! holds business state — every type is a thin shim over a broker handle — so
//! the matcher (T27/T32), orchestrator (T35) and materializer (T28) keep one
//! code path and only swap the adapter between a unit test and production.
//!
//! There is no broker in this sandbox, so this module is *compile-verified*
//! only; the pure header/identity plumbing it does around each hop lives in the
//! shared helpers ([`super::trace`], [`crate::protocol::ids::headers`]) that are
//! tested in their own modules. The behaviour these adapters must reproduce —
//! `Nats-Msg-Id` dedup, ambiguous-publish retries, poison-message handling,
//! bounded pulls — is exercised end to end against the memory bus.
//!
//! Native `async fn` in traits, like the adapter definitions; see the note on
//! [`super::adapter`].
#![allow(async_fn_in_trait)]

use core::future::IntoFuture;
use core::marker::PhantomData;
use core::time::Duration;

use anyhow::anyhow;
use async_nats::HeaderMap;
use async_nats::jetstream::{self, AckKind, consumer::PullConsumer};
use futures::StreamExt;
use tracing::warn;

use crate::bus::adapter::{
    AckHandle, Consumer, Delivery, PublishError, PublishOutcome, Publisher, Source,
};
use crate::bus::{Wire, inbound, last_sent_at};
use crate::protocol::ids::headers::{msg_id_of, stamp_msg_id};

/// The continuous pull-consumer message stream a [`JetStreamSource`] drives.
type PullMessages = jetstream::consumer::pull::Stream;

/// The acknowledgement handle for one JetStream delivery.
///
/// It owns the [`jetstream::Message`] so the ack travels with the message it
/// belongs to, exactly as the trait requires. The stream sequence and delivery
/// count are read once from [`jetstream::Message::info`] at construction — the
/// [`AckHandle::sequence`]/[`AckHandle::deliveries`] accessors are infallible,
/// so the fallible parse cannot live behind them — and cached, while `ack`/`nak`
/// consume the message to make double-acking a compile error.
pub struct JetStreamAck {
    message: jetstream::Message,
    sequence: u64,
    deliveries: u32,
}

impl AckHandle for JetStreamAck {
    async fn ack(self) -> anyhow::Result<()> {
        self.message
            .ack()
            .await
            .map_err(|err| anyhow!("jetstream ack failed: {err}"))
    }

    async fn nak(self, delay: Option<Duration>) -> anyhow::Result<()> {
        self.message
            .ack_with(AckKind::Nak(delay))
            .await
            .map_err(|err| anyhow!("jetstream nak failed: {err}"))
    }

    fn sequence(&self) -> u64 {
        self.sequence
    }

    fn deliveries(&self) -> u32 {
        self.deliveries
    }
}

/// Turn one delivered [`jetstream::Message`] into a decoded [`Delivery`].
///
/// Returns `None` — after acking and warning — when the message cannot be made
/// into an answerable delivery: its JetStream ack metadata is unreadable, or its
/// payload does not decode as a `T`. Acking such a message is deliberate poison
/// handling: it has no identity to answer against and must not loop forever
/// under redelivery (this mirrors `bin/matcher.rs`'s legacy skip, and the
/// `RawBytes` path never decode-fails, so the matcher size-gates the bytes
/// itself). It also stamps the inbound trace/queue-wait span so a producer's
/// send time closes the loop.
async fn build_delivery<T: Wire>(message: jetstream::Message) -> Option<Delivery<T, JetStreamAck>> {
    let subject = message.subject.to_string();
    let headers = message.headers.clone();

    // Continue the producer's trace and record the queue-wait span, then read
    // back the send time it just stashed.
    inbound(&subject, headers.as_ref());
    let sent_at = last_sent_at();
    let msg_id = headers
        .as_ref()
        .and_then(|headers| msg_id_of(headers).map(str::to_owned));

    let (sequence, deliveries) = match message.info() {
        Ok(info) => (info.stream_sequence, info.delivered.max(0) as u32),
        Err(err) => {
            warn!("acking message with unreadable JetStream info on {subject}: {err}");
            let _ = message.ack().await;
            return None;
        }
    };

    let item = match T::decode(message.payload.as_ref()) {
        Ok(item) => item,
        Err(err) => {
            warn!("acking undecodable message on {subject}: {err}");
            let _ = message.ack().await;
            return None;
        }
    };

    Some(Delivery {
        item,
        subject,
        msg_id,
        sent_at,
        redelivered: deliveries > 1,
        handle: JetStreamAck {
            message,
            sequence,
            deliveries,
        },
    })
}

/// A [`Publisher`] over a JetStream [`Context`](jetstream::Context).
///
/// Each publish sets the `Nats-Msg-Id` header to the caller's `msg_id`, so the
/// broker deduplicates a retried ambiguous send within the stream's duplicate
/// window. The publish ack is awaited under [`ack_timeout`](Self::ack_timeout):
/// a timeout is [`PublishError::Ambiguous`] (the message may already be stored,
/// so the caller must retry byte-identically), any other error is
/// [`PublishError::Failed`] (nothing landed).
pub struct JetStreamPublisher<T> {
    context: jetstream::Context,
    ack_timeout: Duration,
    _marker: PhantomData<fn() -> T>,
}

impl<T> JetStreamPublisher<T> {
    /// Wrap a JetStream `context`, awaiting each publish ack for at most
    /// `ack_timeout` before declaring the outcome ambiguous.
    #[must_use]
    pub fn new(context: jetstream::Context, ack_timeout: Duration) -> Self {
        Self {
            context,
            ack_timeout,
            _marker: PhantomData,
        }
    }
}

// Manual `Clone` so `T` need not be `Clone`: the marker is a function pointer,
// and `jetstream::Context` is cheap to clone (it shares the connection).
impl<T> Clone for JetStreamPublisher<T> {
    fn clone(&self) -> Self {
        Self {
            context: self.context.clone(),
            ack_timeout: self.ack_timeout,
            _marker: PhantomData,
        }
    }
}

impl<T: Wire + Send + Sync + 'static> Publisher<T> for JetStreamPublisher<T> {
    async fn publish_bytes(
        &self,
        subject: &str,
        msg_id: &str,
        mut headers: HeaderMap,
        bytes: &[u8],
    ) -> Result<PublishOutcome, PublishError> {
        // The broker dedup key. A retried ambiguous publish carries the same id
        // over identical bytes, so a landed copy collapses instead of doubling.
        stamp_msg_id(&mut headers, msg_id);

        // `Vec<u8>` converts into the broker's `Bytes` payload; the target type
        // is inferred from the parameter, so no `bytes` dependency is named.
        let pending = self
            .context
            .publish_with_headers(subject.to_owned(), headers, bytes.to_vec().into())
            .await
            .map_err(|err| PublishError::Failed(anyhow!("jetstream publish failed: {err}")))?;

        match tokio::time::timeout(self.ack_timeout, pending.into_future()).await {
            // The ack did not arrive in time: the send may or may not have
            // landed, so the caller must retry idempotently.
            Err(_elapsed) => Err(PublishError::Ambiguous(anyhow!(
                "publish ack timed out after {:?}",
                self.ack_timeout
            ))),
            Ok(Err(err)) => Err(PublishError::Failed(anyhow!(
                "jetstream publish ack failed: {err}"
            ))),
            Ok(Ok(ack)) => Ok(PublishOutcome::Acked {
                sequence: ack.sequence,
                duplicate: ack.duplicate,
            }),
        }
    }
}

/// A capacity-bounded [`Consumer`] over a JetStream [`PullConsumer`].
///
/// [`fetch`](Consumer::fetch) claims at most `max` messages, waiting up to
/// `wait` for the batch to fill — exactly the shape the matcher's pull loop
/// wants, where `max` is the number of free solve slots. Undecodable messages
/// are acked and dropped inside [`build_delivery`], so the returned batch holds
/// only answerable deliveries and may be shorter than `max`.
pub struct JetStreamConsumer<T> {
    consumer: PullConsumer,
    _marker: PhantomData<fn() -> T>,
}

impl<T> JetStreamConsumer<T> {
    /// Wrap a durable pull `consumer`.
    #[must_use]
    pub fn new(consumer: PullConsumer) -> Self {
        Self {
            consumer,
            _marker: PhantomData,
        }
    }
}

impl<T: Wire + Send> Consumer<T> for JetStreamConsumer<T> {
    type Handle = JetStreamAck;

    async fn fetch(
        &mut self,
        max: usize,
        wait: Duration,
    ) -> anyhow::Result<Vec<Delivery<T, Self::Handle>>> {
        if max == 0 {
            return Ok(Vec::new());
        }

        // A fetch returns as soon as `max` messages are gathered or `wait`
        // elapses; a shorter batch (or none) is the normal low-load case.
        let mut batch = self
            .consumer
            .fetch()
            .max_messages(max)
            .expires(wait)
            .messages()
            .await?;

        let mut deliveries = Vec::with_capacity(max);
        while let Some(message) = batch.next().await {
            match message {
                Ok(message) => {
                    if let Some(delivery) = build_delivery::<T>(message).await {
                        deliveries.push(delivery);
                    }
                }
                // A pull error ends this batch, not the loop: the caller fetches
                // again next turn. Anything already gathered is still returned.
                Err(err) => {
                    warn!("jetstream fetch batch error: {err}");
                    break;
                }
            }
        }

        Ok(deliveries)
    }
}

/// A push-style [`Source`] over a JetStream pull consumer's message stream.
///
/// This is the shape the orchestrator's per-partition reader (T35) and the
/// materializer's consumer (T28) want: `select!` over the next delivery.
/// Undecodable messages are acked and skipped, so [`next`](Source::next) only
/// yields answerable deliveries; a stream error is surfaced so the caller can
/// decide whether to rebuild the stream.
pub struct JetStreamSource<T> {
    messages: PullMessages,
    _marker: PhantomData<fn() -> T>,
}

impl<T> JetStreamSource<T> {
    /// Wrap an already-open pull-consumer message `stream`.
    #[must_use]
    pub fn new(stream: PullMessages) -> Self {
        Self {
            messages: stream,
            _marker: PhantomData,
        }
    }

    /// Open the continuous message stream of `consumer` and wrap it.
    ///
    /// # Errors
    ///
    /// Returns any error from establishing the pull-consumer message stream.
    pub async fn from_consumer(consumer: &PullConsumer) -> anyhow::Result<Self> {
        let messages = consumer.messages().await?;
        Ok(Self::new(messages))
    }
}

impl<T: Wire + Send> Source<T> for JetStreamSource<T> {
    type Handle = JetStreamAck;

    async fn next(&mut self) -> Option<anyhow::Result<Delivery<T, Self::Handle>>> {
        loop {
            match self.messages.next().await {
                Some(Ok(message)) => match build_delivery::<T>(message).await {
                    // Undecodable: it was acked inside `build_delivery`; skip to
                    // the next message rather than ending the stream.
                    None => continue,
                    Some(delivery) => return Some(Ok(delivery)),
                },
                Some(Err(err)) => return Some(Err(anyhow!("jetstream source error: {err}"))),
                None => return None,
            }
        }
    }
}
