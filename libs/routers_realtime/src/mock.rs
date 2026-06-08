use crate::context::{MatchContext, MatchResult, RawEvent};
use futures::{FutureExt, Sink, Stream, StreamExt, channel::mpsc};
use routers_shard::ShardId;
use std::pin::Pin;
use std::task::{Context, Poll};

pub struct MockSource {
    rx: mpsc::UnboundedReceiver<RawEvent>,
}

pub struct MockSourceHandle {
    tx: mpsc::UnboundedSender<RawEvent>,
}

impl MockSourceHandle {
    pub fn send(&self, event: RawEvent) {
        let _ = self.tx.unbounded_send(event);
    }

    pub fn close(self) {
        drop(self.tx);
    }
}

pub fn mock_source() -> (MockSourceHandle, MockSource) {
    let (tx, rx) = mpsc::unbounded();
    (MockSourceHandle { tx }, MockSource { rx })
}

impl Stream for MockSource {
    type Item = RawEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_next_unpin(cx)
    }
}

pub struct MockSink<S: ShardId> {
    tx: mpsc::UnboundedSender<MatchContext<S>>,
}

pub struct MockSinkReader<S: ShardId> {
    rx: mpsc::UnboundedReceiver<MatchContext<S>>,
}

impl<S: ShardId> MockSinkReader<S> {
    pub async fn recv(&mut self) -> Option<MatchContext<S>> {
        self.rx.next().await
    }

    pub fn try_recv(&mut self) -> Option<MatchContext<S>> {
        self.rx.next().now_or_never().flatten()
    }
}

pub fn mock_sink<S: ShardId>() -> (MockSink<S>, MockSinkReader<S>) {
    let (tx, rx) = mpsc::unbounded();
    (MockSink { tx }, MockSinkReader { rx })
}

impl<S: ShardId + Send> Sink<MatchContext<S>> for MockSink<S> {
    type Error = std::convert::Infallible;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: MatchContext<S>) -> Result<(), Self::Error> {
        let _ = self.tx.unbounded_send(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}

pub struct MockResultSink {
    tx: mpsc::UnboundedSender<MatchResult>,
}

pub struct MockResultReader {
    rx: mpsc::UnboundedReceiver<MatchResult>,
}

impl MockResultReader {
    pub async fn recv(&mut self) -> Option<MatchResult> {
        self.rx.next().await
    }

    pub fn drain(&mut self) -> Vec<MatchResult> {
        let mut out = Vec::new();
        while let Some(r) = self.rx.next().now_or_never().flatten() {
            out.push(r);
        }
        out
    }
}

pub fn mock_result_sink() -> (MockResultSink, MockResultReader) {
    let (tx, rx) = mpsc::unbounded();
    (MockResultSink { tx }, MockResultReader { rx })
}

impl Sink<MatchResult> for MockResultSink {
    type Error = std::convert::Infallible;

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, item: MatchResult) -> Result<(), Self::Error> {
        let _ = self.tx.unbounded_send(item);
        Ok(())
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }
}
