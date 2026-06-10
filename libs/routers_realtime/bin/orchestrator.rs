use routers_realtime::{
    HistoryConfig, MemoryStore, ValkeyStore, WarmingMemoryStore,
    context::{MatchContext, RawEvent},
    metrics,
    nats,
    nats_ingest::{self, NatsIngestOpts},
    orchestrate,
};
use routers_shard::{Geohash, GeohashStrategy};
use tokio::sync::mpsc;

type S = Geohash;

async fn make_source(
    js: async_nats::jetstream::Context,
) -> anyhow::Result<futures::stream::BoxStream<'static, RawEvent>> {
    Ok(Box::pin(nats_ingest::nats_source(js, NatsIngestOpts::from_env()).await?))
}

fn make_sink(
    tx: mpsc::Sender<MatchContext<S>>,
) -> impl futures::Sink<MatchContext<S>, Error = anyhow::Error> {
    futures::sink::unfold(tx, |tx, ctx| async move {
        tx.send(ctx)
            .await
            .map_err(|_| anyhow::anyhow!("NATS publisher task closed"))?;
        Ok::<_, anyhow::Error>(tx)
    })
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
        .unwrap_or(5);
    let parallelism: usize = std::env::var("PARALLELISM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let metrics_addr: std::net::SocketAddr = std::env::var("METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9091".into())
        .parse()
        .expect("METRICS_ADDR must be a valid socket address");
    let nats_batch_size: usize = std::env::var("NATS_BATCH_SIZE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(256);
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
        .map_err(|e| anyhow::anyhow!("NATS match stream setup: {e}"))?;

    let ingest_opts = NatsIngestOpts::from_env();
    nats_ingest::ensure_events_stream(&js, &ingest_opts)
        .await
        .map_err(|e| anyhow::anyhow!("NATS events stream setup: {e}"))?;

    // One batch_publisher per worker to avoid a shared-channel bottleneck.
    let n_publishers = parallelism.max(1);
    let mut txs: Vec<mpsc::Sender<MatchContext<S>>> = Vec::with_capacity(n_publishers);
    for _ in 0..n_publishers {
        let (tx, rx) = mpsc::channel::<MatchContext<S>>(2048);
        tokio::spawn(nats::batch_publisher::<S>(js.clone(), rx, nats_batch_size));
        txs.push(tx);
    }

    match store_mode.as_str() {
        "memory" => {
            eprintln!(
                "store: memory  parallelism=1  history_max_points={history_max_points}"
            );
            let source = make_source(js).await?;
            let tx = txs.remove(0);
            orchestrate(source, make_sink(tx), MemoryStore::<S>::new(valkey_max_len), &strategy, &history_cfg).await
        }
        _ => {
            eprintln!(
                "store: warming ({valkey_url})  parallelism={parallelism}  history_max_points={history_max_points}  nats_batch={nats_batch_size}  valkey_max_len={valkey_max_len}"
            );

            if parallelism == 1 {
                let source = make_source(js).await?;
                let valkey = ValkeyStore::connect(&valkey_url, valkey_max_len).await?;
                let store = WarmingMemoryStore::new(valkey, valkey_max_len);
                let tx = txs.remove(0);
                orchestrate(source, make_sink(tx), store, &strategy, &history_cfg).await
            } else {
                let mut handles = vec![];
                for (i, tx) in txs.into_iter().enumerate() {
                    // All workers share the same NATS consumer name — the server
                    // distributes pending messages across concurrent pull requests.
                    let source = make_source(js.clone()).await?;
                    let valkey = ValkeyStore::connect(&valkey_url, valkey_max_len).await?;
                    let store = WarmingMemoryStore::new(valkey, valkey_max_len);
                    let sink = make_sink(tx);
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
