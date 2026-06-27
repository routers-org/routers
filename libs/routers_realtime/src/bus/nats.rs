use std::pin::Pin;
use std::task::{Context as Ctx, Poll, ready};

use anyhow::Context;
use async_nats::jetstream::{self};
use futures::future::BoxFuture;
use futures::{FutureExt, Sink};
use serde::Serialize;

pub struct JetStreamSink<T: Serialize> {
    client: async_nats::Client, // kept for an explicit flush on shutdown
    js: jetstream::Context,
    subject_of: Box<dyn Fn(&T) -> String>,
    in_flight: Option<BoxFuture<'static, anyhow::Result<()>>>,

    _phantom: std::marker::PhantomData<T>,
}

impl<T: Serialize> JetStreamSink<T> {
    pub fn new(client: async_nats::Client, subject_of: impl Fn(&T) -> String + 'static) -> Self {
        let js = jetstream::new(client.clone());

        Self {
            client,
            js,
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

impl<T: Serialize + Unpin> Sink<T> for JetStreamSink<T> {
    type Error = anyhow::Error;

    fn poll_ready(self: Pin<&mut Self>, cx: &mut Ctx<'_>) -> Poll<Result<(), Self::Error>> {
        self.get_mut().poll_in_flight(cx)
    }

    fn start_send(self: Pin<&mut Self>, item: T) -> Result<(), Self::Error> {
        let this = self.get_mut();
        let payload: Vec<u8> = postcard::to_allocvec(&item).context("failed to serialize")?;

        let subject = this.subject_for(&item);
        let js = this.js.clone();

        this.in_flight = Some(
            async move {
                // resolves once the message is handed to the connection buffer;
                // the returned ack future is dropped (fire-and-forget).
                let _ack = js.publish(subject, payload.into()).await?;
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
