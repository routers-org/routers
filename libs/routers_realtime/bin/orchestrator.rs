use std::time::Duration;

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use geo::{Distance, Haversine};
use log::{debug, info};
use routers_realtime::bus::{NATSSink, NATSStream};
use routers_realtime::event::{MatchContext, Payload, RawEvent};
use routers_realtime::store::{CachedRedisStore, RedisStore};
use url::Url;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server
    #[arg(short, env, long)]
    nats: Url,

    /// URL of the Redis cluster
    #[arg(short, env, long)]
    redis: Url,

    /// The NATS subject to use to source raw events from.
    /// For example, `events.raw.{shard}` where shard is the geohash shard identifier.
    #[arg(short, long = "in", env)]
    inbound_subject: String,

    /// The NATS subject to emit match context messages out into.
    /// For example, `events.match.{shard}` where shard is the geohash shard identifier.
    #[arg(short, long = "out", env)]
    outbound_subject: String,

    /// The number of context entries to retrieve from Redis for each vehicle.
    #[arg(short, long = "context-window", env, default_value = "10")]
    context_window: usize,

    /// Points older than this will be discarded from history, regardless
    /// of if it's within the KV store, or not.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "120s")]
    gap: Duration,

    /// Consecutive points further away than this will be treated as a "teleport",
    /// and dropped along with everything older.
    #[arg(long, env, default_value = "2000")]
    jump: f64,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();
    info!("orchestrator started: {:?}", args);

    let nats_url = ServerAddr::from_url(args.nats).context("could not create NATS url")?;

    let client = ConnectOptions::new()
        .name("OrchestratorService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;

    let mut sink =
        NATSSink::<MatchContext>::new(client.clone(), move |_| args.outbound_subject.clone());

    let subscriber = client
        .subscribe(args.inbound_subject)
        .await
        .context("could not subscribe to NATS subject")?;
    let mut source = NATSStream::<Payload>::new(subscriber);

    let kv = RedisStore::<RawEvent>::new(args.redis)
        .await
        .context("could not connect to redis store")?;

    let mut kv = CachedRedisStore::new(kv);

    let gap = chrono::Duration::from_std(args.gap).context("gap out of range")?;

    while let Some(payload) = source.next().await {
        let mut entries = kv
            .get_many(&payload.vehicle_id, args.context_window)
            .await
            .context("could not get entries from redis store")?;

        // Normalise to newest-first regardless of the datasource's return
        // order: the cutoff below walks back in time from the current event,
        // discarding everything beyond the first gap or teleport.
        entries.sort_by_key(|event| std::cmp::Reverse(event.timestamp));

        let context = entries
            .into_iter()
            .inspect(|v| debug!("event: {:?}", v))
            .scan(
                (payload.point, payload.timestamp),
                |(prev_p, prev_ts), event: RawEvent| {
                    let duration = (*prev_ts - event.timestamp).abs();
                    let distance = Haversine.distance(*prev_p, event.point);

                    if duration <= gap && distance <= args.jump {
                        *prev_p = event.point;
                        *prev_ts = event.timestamp;
                        Some(event)
                    } else {
                        None
                    }
                },
            )
            .collect::<Vec<_>>();

        // Roll the current event into the cached window so the next event's
        // context includes it; Redis only seeds the window on first sight.
        kv.push(&payload.vehicle_id, payload.as_event(), args.context_window);

        // The matcher solves a directed trajectory, so it must receive the
        // points in chronological order or every transition faces backwards.
        // Sort by timestamp rather than assuming the store's return order.
        let mut events: Vec<RawEvent> = std::iter::once(payload.as_event())
            .chain(context)
            .collect();
        events.sort_by_key(|event| event.timestamp);

        let history = events.into_iter().map(|event| event.point).collect();

        sink.send(MatchContext {
            history,
            vehicle_id: payload.vehicle_id,
        })
        .await
        .context("could not send match context")?;
    }

    sink.close().await?;

    Ok(())
}
