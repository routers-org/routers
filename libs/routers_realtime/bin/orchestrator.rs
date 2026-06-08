use chrono::DateTime;
use futures::StreamExt;
use geo::Point;
use routers_realtime::{
    MemoryStore, ValkeyStore,
    amqp::{Topic, TopicOpts},
    context::{MatchContext, RawEvent},
    metrics,
    nats,
    orchestrate,
    orchestrate_batched,
};
use routers_shard::{Geohash, GeohashStrategy};
use tokio::sync::mpsc;

type S = Geohash;

fn make_source(opts: TopicOpts) -> impl std::future::Future<Output = anyhow::Result<impl futures::Stream<Item = RawEvent> + Send>> {
    async move {
        let topic = Topic::new(opts).await?;
        Ok(topic.filter_map(|delivery| async move {
            let delivery = delivery.ok()?;
            let payload: routers_realtime::event::Payload =
                serde_json::from_slice(&delivery.data)
                    .map_err(|e| { eprintln!("[source] JSON parse error: {e}"); e })
                    .ok()?;
            let _ = delivery
                .ack(lapin::options::BasicAckOptions::default())
                .await;
            let ts = DateTime::parse_from_str(&payload.event_time, "%Y-%m-%d %H:%M:%S%.f %Z")
                .or_else(|_| DateTime::parse_from_str(&payload.event_time, "%Y-%m-%d %H:%M:%S %Z"))
                .map(|dt| dt.timestamp_millis() as u64)
                .unwrap_or(0);
            Some(RawEvent {
                vehicle_id: payload.vehicle_id,
                coord: Point::new(payload.point.x, payload.point.y),
                timestamp_ms: ts,
            })
        }))
    }
}

fn make_sink(tx: mpsc::Sender<MatchContext<S>>) -> impl futures::Sink<MatchContext<S>, Error = anyhow::Error> {
    futures::sink::unfold(tx, |tx, ctx| async move {
        tx.send(ctx)
            .await
            .map_err(|_| anyhow::anyhow!("NATS publisher task closed"))?;
        Ok::<_, anyhow::Error>(tx)
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let nats_url = std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let store_mode = std::env::var("STORE").unwrap_or_else(|_| "valkey".into());
    let shard_precision: u8 = std::env::var("SHARD_PRECISION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let valkey_batch_size: usize = std::env::var("VALKEY_BATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(64);
    let valkey_batch_timeout_ms: u64 = std::env::var("VALKEY_BATCH_TIMEOUT_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    // Number of parallel consumer+store pipelines. Each opens its own AMQP connection
    // and Valkey connection, all funnelling into the same shared NATS publisher.
    let parallelism: usize = std::env::var("PARALLELISM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let metrics_addr: std::net::SocketAddr = std::env::var("METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9091".into())
        .parse()
        .expect("METRICS_ADDR must be a valid socket address");

    tokio::spawn(metrics::serve(metrics_addr));

    let strategy = GeohashStrategy::with_precision(shard_precision);

    let nc = async_nats::connect(&nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("NATS connect: {e}"))?;
    let js = async_nats::jetstream::new(nc);
    nats::ensure_match_stream(&js)
        .await
        .map_err(|e| anyhow::anyhow!("NATS stream setup: {e}"))?;

    // One NATS batch publisher shared across all pipeline tasks.
    let (tx, rx) = mpsc::channel::<MatchContext<S>>(2048 * parallelism);
    tokio::spawn(nats::batch_publisher::<S>(js, rx, 64));

    match store_mode.as_str() {
        "memory" => {
            eprintln!("store: memory (no persistence)  parallelism=1");
            let source = make_source(TopicOpts::from_env()).await?;
            orchestrate(source, make_sink(tx), MemoryStore::<S>::new(200), &strategy).await
        }
        _ => {
            eprintln!(
                "store: valkey ({valkey_url})  batch={valkey_batch_size}  timeout={valkey_batch_timeout_ms}ms  parallelism={parallelism}"
            );

            if parallelism == 1 {
                let source = make_source(TopicOpts::from_env()).await?;
                let store = ValkeyStore::connect(&valkey_url).await?;
                orchestrate_batched(
                    source,
                    make_sink(tx),
                    store,
                    strategy,
                    valkey_batch_size,
                    valkey_batch_timeout_ms,
                )
                .await
            } else {
                // Spawn N-1 background tasks, run the last one on the current thread.
                let mut handles = vec![];
                for i in 0..parallelism {
                    let mut opts = TopicOpts::from_env();
                    // Each consumer needs a unique tag within the RabbitMQ connection.
                    opts.consumer_tag = lapin::types::ShortString::from(
                        format!("orchestrator-{i}").as_str(),
                    );
                    let source = make_source(opts).await?;
                    let store = ValkeyStore::connect(&valkey_url).await?;
                    let sink = make_sink(tx.clone());
                    let strat = strategy.clone();
                    let batch_size = valkey_batch_size;
                    let timeout_ms = valkey_batch_timeout_ms;

                    if i < parallelism - 1 {
                        handles.push(tokio::spawn(async move {
                            orchestrate_batched(
                                source, sink, store, strat, batch_size, timeout_ms,
                            )
                            .await
                        }));
                    } else {
                        // Drop the original tx — last task holds its own clone
                        drop(tx);
                        return orchestrate_batched(
                            source, sink, store, strat, batch_size, timeout_ms,
                        )
                        .await;
                    }
                }
                // Wait for the first task to exit (any failure bubbles up)
                if let Some(result) = futures::future::select_all(handles).await.0.ok() {
                    result
                } else {
                    Err(anyhow::anyhow!("orchestrator task panicked"))
                }
            }
        }
    }
}
