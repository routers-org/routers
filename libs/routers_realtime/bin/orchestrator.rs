use routers_realtime::{
    HistoryConfig, MemoryStore, ValkeyStore, WarmingMemoryStore,
    context::RawEvent,
    metrics,
    nats,
    nats_ingest::{self, NatsIngestOpts},
    orchestrate,
};
use routers_shard::{Geohash, GeohashStrategy};

type S = Geohash;

fn make_source(
    js: async_nats::jetstream::Context,
    opts: NatsIngestOpts,
) -> futures::stream::BoxStream<'static, RawEvent> {
    Box::pin(nats_ingest::nats_source(js, opts))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let nats_url =
        std::env::var("NATS_URL").unwrap_or_else(|_| "nats://127.0.0.1:4222".into());
    let valkey_url =
        std::env::var("VALKEY_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into());
    let store_mode = std::env::var("STORE").unwrap_or_else(|_| "valkey".into());
    let shard_precision: u8 = std::env::var("SHARD_PRECISION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(4);
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
        .unwrap_or(25);
    let history_max_age_ms: u64 = std::env::var("HISTORY_MAX_AGE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(300)
        * 1000;
    let valkey_max_len: usize = std::env::var("VALKEY_MAX_LEN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500);
    let owned_shard: Option<String> = std::env::var("OWNED_SHARD").ok();

    let history_cfg = HistoryConfig {
        max_points: history_max_points,
        max_age_ms: history_max_age_ms,
    };

    tokio::spawn(metrics::serve(metrics_addr));

    let strategy = GeohashStrategy::with_precision(shard_precision);

    let nc = async_nats::connect(&nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("NATS connect: {e}"))?;
    let js = async_nats::jetstream::new(nc.clone());

    let mut ingest_opts = NatsIngestOpts::from_env();
    if let Some(ref shard) = owned_shard {
        ingest_opts.subject = format!("events.raw.{}", shard);
        ingest_opts.consumer_name = format!("orchestrator-{}", shard);
    }

    // Remove the legacy MATCH JetStream stream if it exists.
    // Matching now uses core NATS publish/subscribe — JetStream is not needed
    // and would intercept match.* subjects, silently swallowing all match requests.
    let _ = js.delete_stream("MATCH").await;

    nats_ingest::ensure_events_stream(&js, &ingest_opts)
        .await
        .map_err(|e| anyhow::anyhow!("NATS events stream setup: {e}"))?;

    match store_mode.as_str() {
        "memory" => {
            eprintln!(
                "store: memory  parallelism=1  history_max_points={history_max_points}"
            );
            let source = make_source(js, ingest_opts);
            let sink = nats::match_publish_sink::<S>(nc, "match".into());
            orchestrate(source, sink, MemoryStore::<S>::new(valkey_max_len), &strategy, &history_cfg).await
        }
        _ => {
            eprintln!(
                "store: warming ({valkey_url})  parallelism={parallelism}  history_max_points={history_max_points}  valkey_max_len={valkey_max_len}"
            );

            if parallelism == 1 {
                let source = make_source(js, ingest_opts);
                let valkey = ValkeyStore::connect(&valkey_url, valkey_max_len).await?;
                let store = WarmingMemoryStore::new(valkey, valkey_max_len);
                let sink = nats::match_publish_sink::<S>(nc, "match".into());
                orchestrate(source, sink, store, &strategy, &history_cfg).await
            } else {
                let mut handles = vec![];
                for i in 0..parallelism {
                    let source = make_source(js.clone(), ingest_opts.clone());
                    let valkey = ValkeyStore::connect(&valkey_url, valkey_max_len).await?;
                    let store = WarmingMemoryStore::new(valkey, valkey_max_len);
                    let sink = nats::match_publish_sink::<S>(nc.clone(), "match".into());
                    let strat = strategy.clone();
                    let hcfg = history_cfg.clone();

                    if i < parallelism - 1 {
                        handles.push(tokio::spawn(async move {
                            orchestrate(source, sink, store, &strat, &hcfg).await
                        }));
                    } else {
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
