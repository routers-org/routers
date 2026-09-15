//! JetStream implementations of the bus adapters in [`super::adapter`].
//!
//! Each type is a thin shim over a broker handle. There is no broker in this
//! sandbox, so the module is compile-verified only; the behaviour it mirrors is
//! exercised end to end against the memory bus in [`super::memory`].
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
use crate::bus::{Wire, inbound};
use crate::protocol::ids::headers::{msg_id_of, stamp_msg_id};

/// The continuous pull-consumer message stream a [`JetStreamSource`] drives.
type PullMessages = jetstream::consumer::pull::Stream;

/// The acknowledgement handle for one JetStream delivery. Sequence and delivery
/// count are read once from [`jetstream::Message::info`] and cached, since the
/// [`AckHandle`] accessors are infallible.
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
/// Returns `None` — after acking and warning — for unreadable ack metadata or an
/// undecodable payload, preventing poison messages from looping forever.
async fn build_delivery<T: Wire>(message: jetstream::Message) -> Option<Delivery<T, JetStreamAck>> {
    let subject = message.subject.to_string();
    let headers = message.headers.clone();

    let sent_at = inbound(&subject, headers.as_ref());
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
        headers: headers.unwrap_or_default(),
        sent_at,
        redelivered: deliveries > 1,
        handle: JetStreamAck {
            message,
            sequence,
            deliveries,
        },
    })
}

/// A [`Publisher`] over a JetStream [`Context`](jetstream::Context). Each
/// publish stamps `Nats-Msg-Id` for dedup; an ack timeout is
/// [`PublishError::Ambiguous`], any other error [`PublishError::Failed`].
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

// Manual `Clone` so `T` need not be `Clone` (the marker is a function pointer).
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
        stamp_msg_id(&mut headers, msg_id);

        let pending = self
            .context
            .publish_with_headers(subject.to_owned(), headers, bytes.to_vec().into())
            .await
            .map_err(|err| PublishError::Failed(anyhow!("jetstream publish failed: {err}")))?;

        match tokio::time::timeout(self.ack_timeout, pending.into_future()).await {
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
/// `wait`. Undecodable messages are dropped, so the batch may be shorter.
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
                // A pull error ends this batch, not the loop; what is gathered is returned.
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
/// Undecodable messages are acked and skipped; a stream error is surfaced.
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
                    None => continue,
                    Some(delivery) => return Some(Ok(delivery)),
                },
                Some(Err(err)) => return Some(Err(anyhow!("jetstream source error: {err}"))),
                None => return None,
            }
        }
    }
}
