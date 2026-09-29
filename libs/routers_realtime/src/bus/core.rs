//! Transient solve transport. Raw events remain the durable retry source.

use core::marker::PhantomData;
use core::time::Duration;

use async_nats::{Client, HeaderMap, Subscriber};
use futures::{FutureExt as _, StreamExt as _};

use super::Wire;
use super::adapter::{
    AckHandle, Consumer, Delivery, PublishError, PublishOutcome, Publisher, Source,
};
use crate::protocol::ids::headers::{msg_id_of, stamp_msg_id};

/// Core NATS accepts a send into the client buffer; this is not a durable acknowledgement.
pub struct CorePublisher<T> {
    client: Client,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for CorePublisher<T> {
    fn clone(&self) -> Self {
        Self::new(self.client.clone())
    }
}

impl<T> CorePublisher<T> {
    pub fn new(client: Client) -> Self {
        Self {
            client,
            marker: PhantomData,
        }
    }
}

impl<T: Wire + Send + 'static> Publisher<T> for CorePublisher<T> {
    async fn publish_bytes(
        &self,
        subject: &str,
        msg_id: &str,
        mut headers: HeaderMap,
        bytes: &[u8],
    ) -> Result<PublishOutcome, PublishError> {
        stamp_msg_id(&mut headers, msg_id);
        self.client
            .publish_with_headers(subject.to_owned(), headers, bytes.to_vec().into())
            .await
            .map_err(|error| PublishError::Failed(error.into()))?;
        Ok(PublishOutcome::Acked {
            sequence: 0,
            duplicate: false,
        })
    }
}

/// Core messages have no broker acknowledgement. Dropping one is safe only while
/// its source raw event is still unacknowledged and can reconstruct the request.
pub struct CoreAck;

impl AckHandle for CoreAck {
    async fn ack(self) -> anyhow::Result<()> {
        Ok(())
    }
    async fn nak(self, _delay: Option<Duration>) -> anyhow::Result<()> {
        Ok(())
    }
    fn sequence(&self) -> u64 {
        0
    }
    fn deliveries(&self) -> u32 {
        1
    }
}

pub struct CoreSource<T> {
    subscriber: Subscriber,
    queue: Option<(Client, String, String)>,
    direct: Option<(Client, String, Subscriber)>,
    marker: PhantomData<fn() -> T>,
}

impl<T> CoreSource<T> {
    pub fn new(subscriber: Subscriber) -> Self {
        Self {
            subscriber,
            queue: None,
            direct: None,
            marker: PhantomData,
        }
    }

    /// Remember a regional queue subscription so a closed subscriber can rejoin.
    pub fn with_queue(mut self, client: Client, subject: String, group: String) -> Self {
        self.queue = Some((client, subject, group));
        self
    }

    /// Also receive from this process's own, non-queued `subject`: requests an
    /// owner addressed to this replica because it holds their cached state.
    pub fn with_direct(mut self, client: Client, subject: String, subscriber: Subscriber) -> Self {
        self.direct = Some((client, subject, subscriber));
        self
    }

    /// The next message from either subscription, preferring directed work.
    /// `Err(true)` means the queue subscription closed; `Err(false)` the direct one.
    async fn next_message(&mut self) -> Result<async_nats::Message, bool> {
        match &mut self.direct {
            Some((_, _, direct)) => tokio::select! {
                biased;
                message = direct.next() => message.ok_or(false),
                message = self.subscriber.next() => message.ok_or(true),
            },
            None => self.subscriber.next().await.ok_or(true),
        }
    }

    /// A message already buffered on either subscription, without waiting.
    fn buffered_message(&mut self) -> Option<async_nats::Message> {
        if let Some((_, _, direct)) = &mut self.direct
            && let Some(Some(message)) = direct.next().now_or_never()
        {
            return Some(message);
        }
        self.subscriber.next().now_or_never().flatten()
    }

    /// Re-establish whichever subscription closed.
    async fn rejoin(&mut self, queue_closed: bool) -> anyhow::Result<()> {
        if queue_closed {
            let Some((client, subject, group)) = &self.queue else {
                anyhow::bail!("core subscription closed");
            };
            self.subscriber = client
                .queue_subscribe(subject.clone(), group.clone())
                .await?;
            client.flush().await?;
        } else if let Some((client, subject, subscriber)) = &mut self.direct {
            *subscriber = client.subscribe(subject.clone()).await?;
            client.flush().await?;
        }
        Ok(())
    }
}

fn delivery<T: Wire>(message: async_nats::Message) -> anyhow::Result<Delivery<T, CoreAck>> {
    let headers = message.headers.unwrap_or_default();
    let msg_id = msg_id_of(&headers).map(str::to_owned);
    let item = T::decode(&message.payload)?;
    Ok(Delivery {
        item,
        handle: CoreAck,
        subject: message.subject.to_string(),
        msg_id,
        headers,
        sent_at: None,
        redelivered: false,
    })
}

impl<T: Wire + Send> Source<T> for CoreSource<T> {
    type Handle = CoreAck;

    async fn next(&mut self) -> Option<anyhow::Result<Delivery<T, Self::Handle>>> {
        loop {
            match self.next_message().await {
                Ok(message) => return Some(delivery(message)),
                // A plain source ends with its subscription; a directed one is
                // an auxiliary stream that rejoins.
                Err(true) => return None,
                Err(false) => {
                    if let Err(error) = self.rejoin(false).await {
                        return Some(Err(error));
                    }
                }
            }
        }
    }
}

impl<T: Wire + Send> Consumer<T> for CoreSource<T> {
    type Handle = CoreAck;

    async fn fetch(
        &mut self,
        max: usize,
        wait: Duration,
    ) -> anyhow::Result<Vec<Delivery<T, Self::Handle>>> {
        let mut batch = Vec::with_capacity(max);
        if max == 0 {
            return Ok(batch);
        }
        let Ok(first) = tokio::time::timeout(wait, self.next_message()).await else {
            return Ok(batch);
        };
        match first {
            Ok(first) => batch.push(delivery(first)?),
            Err(queue_closed) => {
                self.rejoin(queue_closed).await?;
                return Ok(batch);
            }
        }
        while batch.len() < max {
            match self.buffered_message() {
                Some(message) => batch.push(delivery(message)?),
                None => break,
            }
        }
        Ok(batch)
    }
}
