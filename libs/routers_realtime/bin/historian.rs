use chrono::{DateTime, NaiveDateTime};
use futures::StreamExt;
use geo::Point;
use routers_realtime::{
    ValkeyStore,
    amqp::{Topic, TopicOpts},
    context::Position,
    event,
};
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use tokio::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let shard_precision: u8 = std::env::var("SHARD_PRECISION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
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

    let mut opts = TopicOpts::from_env();
    // Historian uses its own durable queue so it receives every message
    // independently of the orchestrator's queue.
    opts = opts.with_queue(
        &std::env::var("RABBITMQ_QUEUE").unwrap_or_else(|_| "historian".into()),
    );
    opts.consumer_tag = lapin::types::ShortString::from("historian");

    let topic = Topic::new(opts).await?;
    futures::pin_mut!(topic);

    eprintln!("historian: valkey={valkey_url}  batch={batch_size}  timeout={batch_timeout_ms}ms");

    let timeout = Duration::from_millis(batch_timeout_ms);
    let mut batch: Vec<(String, Geohash, Position)> = Vec::with_capacity(batch_size);

    loop {
        batch.clear();

        // Block until first message arrives.
        let first = match topic.next().await {
            None => break,
            Some(Ok(d)) => d,
            Some(Err(e)) => {
                eprintln!("historian: AMQP error: {e}");
                break;
            }
        };

        if let Some((vehicle_id, shard, position)) = parse_delivery(&first.data, &strategy) {
            batch.push((vehicle_id, shard, position));
        }
        let _ = first.ack(lapin::options::BasicAckOptions::default()).await;

        // Fill batch until timeout or capacity.
        let deadline = Instant::now() + timeout;
        while batch.len() < batch_size {
            match tokio::time::timeout_at(deadline, topic.next()).await {
                Ok(Some(Ok(delivery))) => {
                    if let Some(entry) = parse_delivery(&delivery.data, &strategy) {
                        batch.push(entry);
                    }
                    let _ = delivery.ack(lapin::options::BasicAckOptions::default()).await;
                }
                _ => break,
            }
        }

        if let Err(e) = valkey.write_many(&batch).await {
            eprintln!("historian: Valkey write error: {e}");
        }
    }

    Ok(())
}

fn parse_delivery(
    data: &[u8],
    strategy: &GeohashStrategy,
) -> Option<(String, Geohash, Position)> {
    let payload: event::Payload = serde_json::from_slice(data)
        .map_err(|e| eprintln!("historian: JSON parse error: {e}"))
        .ok()?;

    let ts = DateTime::parse_from_str(&payload.event_time, "%Y-%m-%d %H:%M:%S%.f %Z")
        .or_else(|_| DateTime::parse_from_str(&payload.event_time, "%Y-%m-%d %H:%M:%S %Z"))
        .map(|dt| dt.timestamp_millis() as u64)
        .unwrap_or_else(|_| {
            let s = payload.event_time.trim_end_matches(" UTC");
            NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
                .map(|dt| dt.and_utc().timestamp_millis() as u64)
                .unwrap_or(0)
        });

    let coord = Point::new(payload.point.x, payload.point.y);
    let shard = strategy.locate(coord);
    let position = Position {
        coord,
        timestamp_ms: ts,
    };
    Some((payload.vehicle_id, shard, position))
}
