use futures::StreamExt;
use routers_realtime::{
    ValkeyStore,
    context::Position,
    nats_ingest::{self, NatsIngestOpts},
};
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use tokio::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let shard_precision: u8 = std::env::var("SHARD_PRECISION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
    let batch_size: usize = std::env::var("VALKEY_BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    let batch_timeout_ms: u64 = std::env::var("VALKEY_BATCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let valkey_max_len: usize = std::env::var("VALKEY_MAX_LEN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);

    let mut valkey = ValkeyStore::connect(&valkey_url, valkey_max_len).await?;
    let strategy = GeohashStrategy::with_precision(shard_precision);

    let nc = async_nats::connect(&nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("NATS connect: {e}"))?;
    let js = async_nats::jetstream::new(nc);

    // Uses EVENTS_CONSUMER env var (default "historian") — a separate durable
    // consumer from the orchestrator's so every event is delivered to both.
    let opts = NatsIngestOpts::from_env();
    nats_ingest::ensure_events_stream(&js, &opts).await?;
    let mut events = Box::pin(nats_ingest::nats_source(js, opts));

    eprintln!(
        "historian: nats={nats_url}  valkey={valkey_url}  batch={batch_size}  timeout={batch_timeout_ms}ms"
    );

    let timeout = Duration::from_millis(batch_timeout_ms);
    let mut batch: Vec<(String, Geohash, Position)> = Vec::with_capacity(batch_size);

    loop {
        batch.clear();

        let first = match events.next().await {
            None => break,
            Some(e) => e,
        };
        batch.push((
            first.vehicle_id,
            strategy.locate(first.coord),
            Position { coord: first.coord, timestamp_ms: first.timestamp_ms, resolved_at_ms: 0 },
        ));

        let deadline = Instant::now() + timeout;
        while batch.len() < batch_size {
            match tokio::time::timeout_at(deadline, events.next()).await {
                Ok(Some(event)) => batch.push((
                    event.vehicle_id,
                    strategy.locate(event.coord),
                    Position { coord: event.coord, timestamp_ms: event.timestamp_ms, resolved_at_ms: 0 },
                )),
                _ => break,
            }
        }

        if let Err(e) = valkey.write_many(&batch).await {
            eprintln!("historian: Valkey write error: {e}");
        }
    }

    Ok(())
}
