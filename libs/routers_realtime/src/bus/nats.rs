use std::pin::Pin;
use std::task::{Context as Ctx, Poll, ready};

use anyhow::Context;
use futures::future::BoxFuture;
use futures::{FutureExt, Sink, Stream, StreamExt};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct NATSSink<T: Serialize> {
    client: async_nats::Client,
    subject_of: Box<dyn Fn(&T) -> String + Send + Sync>,
    in_flight: Option<BoxFuture<'static, anyhow::Result<()>>>,

    _phantom: std::marker::PhantomData<T>,
}

pub struct NATSStream<T: DeserializeOwned> {
    subscriber: Option<async_nats::Subscriber>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Serialize> NATSSink<T> {
    pub fn new(
        client: async_nats::Client,
        subject_of: impl Fn(&T) -> String + Send + Sync + 'static,
    ) -> Self {
        Self {
            client,
            subject_of: Box::new(subject_of),
            in_flight: None,
            _phantom: std::marker::PhantomData,
        }
    }

    pub fn client(&self) -> &async_nats::Client {
        &self.client
    }

    fn subject_for(&self, item: &T) -> String {
        (self.subject_of)(item)
    }

    /// Drive the current publish to completion (slot becomes free).
    fn poll_in_flight(&mut self, cx: &mut Ctx<'_>) -> Poll<Result<(), anyhow::Error>> {
        if let Some(fut) = self.in_flight.as_mut() {
            ready!(fut.poll_unpin(cx))?;
            self.in_flight = None;
        }
        Poll::Ready(Ok(()))
    }
}

impl<T: Serialize + Unpin> Sink<T> for NATSSink<T> {
    type Error = anyhow::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Ctx<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().poll_in_flight(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let payload: Vec<u8> = postcard::to_allocvec(&item).context("failed to serialize")?;

        let subject = this.subject_for(&item);
        let client = this.client.clone();

        // Stamp the message with the sending span's trace context and the
        // send time, so the consumer can measure queue wait and continue
        // the trace (see `bus::trace`).
        let headers = super::trace::outbound();

        this.in_flight = Some(
            async move {
                client
                    .publish_with_headers(subject, headers, payload.into())
                    .await?;
                Ok::<(), anyhow::Error>(())
            }
            .boxed(),
        );
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Ctx<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().poll_in_flight(cx)
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Ctx<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().poll_in_flight(cx)
    }
}

impl<T: DeserializeOwned> NATSStream<T> {
    pub fn new(subscriber: async_nats::Subscriber) -> Self {
        Self {
            subscriber: Some(subscriber),
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T> Stream for NATSStream<T>
where
    T: Serialize + DeserializeOwned + Unpin,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Ctx<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        let Some(subscriber) = &mut this.subscriber else {
            return Poll::Ready(None);
        };

        loop {
            match ready!(subscriber.poll_next_unpin(cx)) {
                Some(message) => match postcard::from_bytes(&message.payload) {
                    Ok(item) => {
                        // Close the publisher's timing loop: the gap between
                        // its send stamp and now is the queue-wait span.
                        super::trace::inbound(message.subject.as_str(), message.headers.as_ref());
                        return Poll::Ready(Some(item));
                    }
                    // A message that isn't a `T` (e.g. a foreign publisher on
                    // the same subject) must not end the stream: skip it.
                    Err(err) => {
                        super::trace::dropped(message.subject.as_str(), "undecodable");
                        log::warn!("skipping undecodable message on {}: {err}", message.subject);
                    }
                },
                None => return Poll::Ready(None),
            }
        }
    }
}
