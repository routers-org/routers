use chrono::{DateTime, NaiveDateTime};
use futures::StreamExt;
use geo::Point;
use routers_realtime::{
    HistoryConfig, MemoryStore, ValkeyStore, WarmingMemoryStore,
    amqp::{Topic, TopicOpts},
    context::{MatchContext, RawEvent},
    metrics,
    nats,
    orchestrate,
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
                .unwrap_or_else(|_| {
                    let s = payload.event_time.trim_end_matches(" UTC");
                    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
                        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S"))
                        .map(|dt| dt.and_utc().timestamp_millis() as u64)
                        .unwrap_or(0)
                });
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
    let parallelism: usize = std::env::var("PARALLELISM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let metrics_addr: std::net::SocketAddr = std::env::var("METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9091".into())
        .parse()
        .expect("METRICS_ADDR must be a valid socket address");

    let history_max_points: usize = std::env::var("HISTORY_MAX_POINTS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let history_max_age_ms: u64 = std::env::var("HISTORY_MAX_AGE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(300)
        * 1000;
    let valkey_max_len: usize = std::env::var("VALKEY_MAX_LEN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);

    let history_cfg = HistoryConfig {
        max_points: history_max_points,
        max_age_ms: history_max_age_ms,
    };

    tokio::spawn(metrics::serve(metrics_addr));

    let strategy = GeohashStrategy::with_precision(shard_precision);

    let nc = async_nats::connect(&nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("NATS connect: {e}"))?;
    let js = async_nats::jetstream::new(nc);
    nats::ensure_match_stream(&js)
        .await
        .map_err(|e| anyhow::anyhow!("NATS stream setup: {e}"))?;

    let (tx, rx) = mpsc::channel::<MatchContext<S>>(2048 * parallelism);
    tokio::spawn(nats::batch_publisher::<S>(js, rx, 64));

    match store_mode.as_str() {
        "memory" => {
            eprintln!("store: memory (no persistence)  parallelism=1  history_max_points={history_max_points}");
            let source = make_source(TopicOpts::from_env()).await?;
            orchestrate(source, make_sink(tx), MemoryStore::<S>::new(valkey_max_len), &strategy, &history_cfg).await
        }
        _ => {
            eprintln!(
                "store: warming ({valkey_url})  parallelism={parallelism}  history_max_points={history_max_points}  valkey_max_len={valkey_max_len}"
            );

            if parallelism == 1 {
                let source = make_source(TopicOpts::from_env()).await?;
                let valkey = ValkeyStore::connect(&valkey_url, valkey_max_len).await?;
                let store = WarmingMemoryStore::new(valkey, valkey_max_len);
                orchestrate(source, make_sink(tx), store, &strategy, &history_cfg).await
            } else {
                let mut handles = vec![];
                for i in 0..parallelism {
                    let mut opts = TopicOpts::from_env();
                    opts.consumer_tag = lapin::types::ShortString::from(
                        format!("orchestrator-{i}").as_str(),
                    );
                    let source = make_source(opts).await?;
                    let valkey = ValkeyStore::connect(&valkey_url, valkey_max_len).await?;
                    let store = WarmingMemoryStore::new(valkey, valkey_max_len);
                    let sink = make_sink(tx.clone());
                    let strat = strategy.clone();

                    let hcfg = history_cfg.clone();
                    if i < parallelism - 1 {
                        handles.push(tokio::spawn(async move {
                            orchestrate(source, sink, store, &strat, &hcfg).await
                        }));
                    } else {
                        drop(tx);
                        return orchestrate(source, sink, store, &strat, &hcfg).await;
                    }
                }
                if let Some(result) = futures::future::select_all(handles).await.0.ok() {
                    result
                } else {
                    Err(anyhow::anyhow!("orchestrator task panicked"))
                }
            }
        }
    }
}
