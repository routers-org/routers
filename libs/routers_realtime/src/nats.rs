use crate::context::{MatchContext, MatchResult, MatchRoute};
use futures::Sink;
use routers_shard::ShardId;
use serde::Serialize;
use std::fmt;
use thiserror::Error;
use tokio::sync::mpsc;

#[derive(Debug, Error)]
pub enum NatsSinkError {
    #[error("serialization failed: {0}")]
    Serialize(#[from] postcard::Error),
    #[error("publish failed")]
    Publish,
}

pub fn match_sink<S>(
    js: async_nats::jetstream::Context,
    subject_prefix: String,
) -> impl Sink<MatchContext<S>, Error = NatsSinkError>
where
    S: ShardId + fmt::Display + Serialize + Send + 'static,
{
    futures::sink::unfold(js, move |js, ctx: MatchContext<S>| {
        let subject = format!("{}.{}", subject_prefix, ctx.target_shard);
        async move {
            let payload: bytes::Bytes = postcard::to_allocvec(&ctx)
                .map_err(NatsSinkError::Serialize)?
                .into();
            js.publish(subject, payload)
                .await
                .map_err(|_| NatsSinkError::Publish)?
                .await
                .map_err(|_| NatsSinkError::Publish)?;
            Ok(js)
        }
    })
}

pub fn result_sink(
    nc: async_nats::Client,
    subject: String,
) -> impl Sink<MatchResult, Error = NatsSinkError> {
    futures::sink::unfold(nc, move |nc, result: MatchResult| {
        let subject = subject.clone();
        async move {
            let payload: bytes::Bytes = postcard::to_allocvec(&result)
                .map_err(NatsSinkError::Serialize)?
                .into();
            nc.publish(subject, payload)
                .await
                .map_err(|_| NatsSinkError::Publish)?;
            Ok(nc)
        }
    })
}

/// Fire-and-forget sink for [`MatchRoute`] to `matched.routes.{vehicle_id}`.
///
/// Subject is `{subject_prefix}.{vehicle_id}`. Callers typically pass
/// `"matched.routes"` as the prefix.
pub fn route_sink(
    nc: async_nats::Client,
    subject_prefix: String,
) -> impl Sink<MatchRoute, Error = NatsSinkError> {
    futures::sink::unfold(nc, move |nc, route: MatchRoute| {
        let subject = format!("{}.{}", subject_prefix, route.vehicle_id);
        async move {
            let payload: bytes::Bytes = postcard::to_allocvec(&route)
                .map_err(NatsSinkError::Serialize)?
                .into();
            nc.publish(subject, payload)
                .await
                .map_err(|_| NatsSinkError::Publish)?;
            Ok(nc)
        }
    })
}

pub async fn ensure_match_stream(
    js: &async_nats::jetstream::Context,
) -> Result<async_nats::jetstream::stream::Stream, async_nats::Error> {
    let config = async_nats::jetstream::stream::Config {
        name: "MATCH".into(),
        subjects: vec!["match.>".into()],
        // Memory storage: no disk I/O per message — essential for high throughput.
        // File storage caps at ~20k msg/s on a local VM due to fsync overhead.
        storage: async_nats::jetstream::stream::StorageType::Memory,
        // Drop oldest messages when full so publish never blocks.
        max_bytes: 512 * 1024 * 1024,
        discard: async_nats::jetstream::stream::DiscardPolicy::Old,
        ..Default::default()
    };
    // Storage type cannot be changed in place — if the existing stream uses File,
    // delete it and recreate. Message loss is acceptable: unprocessed messages
    // are from a previous run and will be replayed by the orchestrator.
    match js.update_stream(&config).await {
        Ok(_) => js.get_stream("MATCH").await.map_err(Into::into),
        Err(_) => {
            let _ = js.delete_stream("MATCH").await;
            js.create_stream(config).await.map_err(Into::into)
        }
    }
}

/// Receives [`MatchContext`] items from a channel, batches them, fires all
/// NATS publishes without waiting for individual acks, then awaits all acks
/// together. This decouples the orchestrate loop from NATS round-trip latency.
pub async fn batch_publisher<S>(
    js: async_nats::jetstream::Context,
    mut rx: mpsc::Receiver<MatchContext<S>>,
    batch_size: usize,
) where
    S: ShardId + fmt::Display + Serialize + Send + 'static,
{
    let m = crate::metrics::global();
    let mut batch: Vec<MatchContext<S>> = Vec::with_capacity(batch_size);

    loop {
        batch.clear();

        // Block until at least one item is ready.
        match rx.recv().await {
            None => break,
            Some(ctx) => batch.push(ctx),
        }

        // Drain whatever else is immediately available.
        while batch.len() < batch_size {
            match rx.try_recv() {
                Ok(ctx) => batch.push(ctx),
                Err(_) => break,
            }
        }

        let t0 = std::time::Instant::now();

        // Fire all publishes without waiting for individual acks.
        let mut pending: Vec<async_nats::jetstream::context::PublishAckFuture> =
            Vec::with_capacity(batch.len());
        let mut n = 0usize;
        for ctx in &batch {
            let subject = format!("match.{}", ctx.target_shard);
            let Ok(payload) = postcard::to_allocvec(ctx).map(bytes::Bytes::from) else {
                continue;
            };
            match js.publish(subject, payload).await {
                Ok(ack) => {
                    pending.push(ack);
                    n += 1;
                }
                Err(e) => eprintln!("[batch_publisher] NATS publish error: {e}"),
            }
        }

        // Await all acks together — one RTT for the whole batch.
        for ack in pending {
            if let Err(e) = ack.await {
                eprintln!("[batch_publisher] NATS ack error: {e}");
                n = n.saturating_sub(1);
            }
        }

        if n > 0 {
            let per_event_ms = t0.elapsed().as_secs_f64() * 1000.0 / n as f64;
            for _ in 0..n {
                m.nats_latency_ms.observe(per_event_ms);
                m.events_published.inc();
            }
        }
    }
}

pub async fn match_consumer<S>(
    js: &async_nats::jetstream::Context,
    shard: &S,
) -> Result<
    async_nats::jetstream::consumer::Consumer<
        async_nats::jetstream::consumer::pull::Config,
    >,
    async_nats::Error,
>
where
    S: ShardId + fmt::Display,
{
    let stream = ensure_match_stream(js).await?;
    stream
        .get_or_create_consumer(
            &format!("matchers-{}", shard),
            async_nats::jetstream::consumer::pull::Config {
                durable_name: Some(format!("matchers-{}", shard)),
                filter_subject: format!("match.{}", shard),
                ack_policy: async_nats::jetstream::consumer::AckPolicy::Explicit,
                ..Default::default()
            },
        )
        .await
        .map_err(Into::into)
}
