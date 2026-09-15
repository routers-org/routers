use std::pin::Pin;
use std::task::{Context as Ctx, Poll, ready};

use futures::{Stream, StreamExt};

use super::Wire;

pub struct NATSStream<T: Wire> {
    subscriber: Option<async_nats::Subscriber>,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Wire> NATSStream<T> {
    pub fn new(subscriber: async_nats::Subscriber) -> Self {
        Self {
            subscriber: Some(subscriber),
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<T> Stream for NATSStream<T>
where
    T: Wire + Unpin,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Ctx<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();

        let Some(subscriber) = &mut this.subscriber else {
            return Poll::Ready(None);
        };

        loop {
            match ready!(subscriber.poll_next_unpin(cx)) {
                Some(message) => match T::decode(&message.payload) {
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
