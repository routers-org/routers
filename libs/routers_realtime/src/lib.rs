pub mod amqp;
pub mod assignment;
pub mod context;
pub mod event;
pub mod history;
pub mod metrics;
pub mod mock;
pub mod nats;
pub mod store;

use context::{MatchContext, Position, RawEvent};
use futures::{Sink, SinkExt, Stream, StreamExt};
use routers_shard::{ShardId, ShardingStrategy};
use serde::{Serialize, de::DeserializeOwned};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::time::{Duration, Instant};
pub use store::{MemoryStore, ValkeyStore};
use store::PositionStore;

pub async fn orchestrate<St, Si, P, Strat, S>(
    source: St,
    sink: Si,
    mut store: P,
    strategy: &Strat,
) -> anyhow::Result<()>
where
    St: Stream<Item = RawEvent>,
    Si: Sink<MatchContext<S>>,
    Si::Error: std::fmt::Display,
    P: PositionStore<S>,
    Strat: ShardingStrategy<Id = S>,
    S: ShardId + Clone,
{
    futures::pin_mut!(source);
    futures::pin_mut!(sink);

    let m = metrics::global();

    while let Some(event) = source.next().await {
        m.events_received.inc();
        let t_event = std::time::Instant::now();

        let resolved_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;

        if event.timestamp_ms > 0 {
            let age = resolved_at_ms.saturating_sub(event.timestamp_ms) as f64;
            m.event_age_ms.observe(age);
        }

        let shard = strategy.locate(event.coord);
        let position = Position {
            coord: event.coord,
            timestamp_ms: event.timestamp_ms,
        };

        let t_store = std::time::Instant::now();
        let raw_history = match store
            .push_and_fetch(&event.vehicle_id, shard.clone(), position.clone())
            .await
        {
            Ok(h) => h,
            Err(e) => {
                m.store_errors.inc();
                return Err(anyhow::anyhow!("store error: {e}"));
            }
        };
        m.store_latency_ms
            .observe(t_store.elapsed().as_secs_f64() * 1000.0);

        let history = history::filter_history(raw_history.into_iter());
        let ctx = MatchContext {
            vehicle_id: event.vehicle_id,
            resolved_at_ms,
            history,
            current: position,
            target_shard: shard,
        };

        // total_latency_ms covers everything up to the sink hand-off.
        // When the sink is a channel (batched NATS publisher), this is
        // Valkey RTT + CPU. nats_latency_ms and events_published are
        // recorded by the publisher task after acks arrive.
        sink.send(ctx)
            .await
            .map_err(|e| anyhow::anyhow!("sink error: {e}"))?;
        m.total_latency_ms
            .observe(t_event.elapsed().as_secs_f64() * 1000.0);
    }

    Ok(())
}

/// Like [`orchestrate`] but pipelines N Valkey writes in one round-trip.
///
/// Collects up to `batch_size` events from `source` (waiting at most
/// `batch_timeout_ms` for the batch to fill after the first event arrives),
/// then issues all N XADD + N XREVRANGE commands in a single pipeline.
/// This trades per-event Valkey latency for per-batch latency, reducing
/// Valkey round-trips from N to 1.
pub async fn orchestrate_batched<St, Si, Strat, S>(
    source: St,
    sink: Si,
    mut store: ValkeyStore,
    strategy: Strat,
    batch_size: usize,
    batch_timeout_ms: u64,
) -> anyhow::Result<()>
where
    St: Stream<Item = RawEvent>,
    Si: Sink<MatchContext<S>>,
    Si::Error: std::fmt::Display,
    Strat: ShardingStrategy<Id = S>,
    S: ShardId + Clone + Serialize + DeserializeOwned,
{
    futures::pin_mut!(source);
    futures::pin_mut!(sink);

    let m = metrics::global();
    let timeout = Duration::from_millis(batch_timeout_ms);

    // (vehicle_id, shard, position, resolved_at_ms, timestamp_ms)
    let mut batch: Vec<(String, S, Position, u64, u64)> = Vec::with_capacity(batch_size);

    loop {
        batch.clear();

        // Block until first event arrives.
        let first = match source.next().await {
            None => break,
            Some(e) => e,
        };

        let resolved_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let shard = strategy.locate(first.coord);
        let position = Position {
            coord: first.coord,
            timestamp_ms: first.timestamp_ms,
        };
        m.events_received.inc();
        if first.timestamp_ms > 0 {
            m.event_age_ms
                .observe(resolved_at_ms.saturating_sub(first.timestamp_ms) as f64);
        }
        batch.push((first.vehicle_id, shard, position, resolved_at_ms, first.timestamp_ms));

        // Fill batch until timeout or capacity reached.
        let deadline = Instant::now() + timeout;
        while batch.len() < batch_size {
            match tokio::time::timeout_at(deadline, source.next()).await {
                Ok(Some(event)) => {
                    let resolved_at_ms = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap()
                        .as_millis() as u64;
                    let shard = strategy.locate(event.coord);
                    let position = Position {
                        coord: event.coord,
                        timestamp_ms: event.timestamp_ms,
                    };
                    m.events_received.inc();
                    if event.timestamp_ms > 0 {
                        m.event_age_ms
                            .observe(resolved_at_ms.saturating_sub(event.timestamp_ms) as f64);
                    }
                    batch.push((event.vehicle_id, shard, position, resolved_at_ms, event.timestamp_ms));
                }
                _ => break,
            }
        }

        // One Valkey round-trip for the whole batch.
        let store_input: Vec<(String, S, Position)> = batch
            .iter()
            .map(|(vid, shard, pos, _, _)| (vid.clone(), shard.clone(), pos.clone()))
            .collect();

        let t_store = std::time::Instant::now();
        let histories = match store.push_and_fetch_many(&store_input).await {
            Ok(h) => h,
            Err(e) => {
                m.store_errors.inc();
                return Err(anyhow::anyhow!("store error: {e}"));
            }
        };
        let store_elapsed_ms = t_store.elapsed().as_secs_f64() * 1000.0;
        let per_event_store_ms = store_elapsed_ms / batch.len() as f64;

        for ((vehicle_id, shard, position, resolved_at_ms, _), raw_history) in
            batch.iter().zip(histories.into_iter())
        {
            let t_event = std::time::Instant::now();
            m.store_latency_ms.observe(per_event_store_ms);

            let history = history::filter_history(raw_history.into_iter());
            let ctx = MatchContext {
                vehicle_id: vehicle_id.clone(),
                resolved_at_ms: *resolved_at_ms,
                history,
                current: position.clone(),
                target_shard: shard.clone(),
            };

            sink.send(ctx)
                .await
                .map_err(|e| anyhow::anyhow!("sink error: {e}"))?;
            m.total_latency_ms
                .observe(t_event.elapsed().as_secs_f64() * 1000.0);
        }
    }

    Ok(())
}
