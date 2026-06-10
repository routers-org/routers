pub mod assignment;
pub mod context;
pub mod event;
pub mod history;
pub mod metrics;
pub mod mock;
pub mod nats;
pub mod nats_ingest;
pub mod store;

use context::{MatchContext, Position, RawEvent};
use futures::{Sink, SinkExt, Stream, StreamExt};
use routers_shard::{ShardId, ShardingStrategy};
use std::time::{SystemTime, UNIX_EPOCH};
pub use store::{MemoryStore, ValkeyStore, WarmingMemoryStore};
use store::PositionStore;

/// Controls how much position history is passed to each matcher.
///
/// Both parameters affect HMM quality: longer sequences give the transition
/// model more signal, but also increase per-solve CPU cost. 10 points covers
/// ~30–60 s of typical GPS cadence and is sufficient for urban networks.
#[derive(Clone)]
pub struct HistoryConfig {
    /// Maximum number of positions included in each `MatchContext`.
    pub max_points: usize,
    /// Positions older than this (relative to the current event) are dropped.
    pub max_age_ms: u64,
}

impl Default for HistoryConfig {
    fn default() -> Self {
        Self {
            max_points: 10,
            max_age_ms: 5 * 60 * 1000,
        }
    }
}

pub async fn orchestrate<St, Si, P, Strat, S>(
    source: St,
    sink: Si,
    mut store: P,
    strategy: &Strat,
    history_cfg: &HistoryConfig,
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
            m.event_age_latest_s.set(age / 1000.0);
        }

        let shard = strategy.locate(event.coord);
        let position = Position {
            coord: event.coord,
            timestamp_ms: event.timestamp_ms,
            resolved_at_ms,
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

        // raw_history[0] is the position we just wrote; skip it so history only
        // contains prior events and ctx.current isn't duplicated in the linestring.
        let history = history::filter_history(raw_history.into_iter().skip(1), history_cfg.max_points, event.timestamp_ms, history_cfg.max_age_ms);
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
        let t_nats = std::time::Instant::now();
        sink.send(ctx)
            .await
            .map_err(|e| anyhow::anyhow!("sink error: {e}"))?;
        m.nats_latency_ms
            .observe(t_nats.elapsed().as_secs_f64() * 1000.0);
        m.events_published.inc();
        m.total_latency_ms
            .observe(t_event.elapsed().as_secs_f64() * 1000.0);
    }

    Ok(())
}

