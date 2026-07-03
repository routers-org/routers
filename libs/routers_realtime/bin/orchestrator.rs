use std::time::Duration;

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use geo::{Distance, Haversine};
use log::info;
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

    while let Some(payload) = source.next().await {
        let context = kv
            .get_many(&payload.vehicle_id, args.context_window)
            .await
            .context("could not get entries from redis store")?
            .into_iter()
            .scan(
                (payload.point, payload.event_ms),
                |(prev_p, prev_ms),
                 RawEvent {
                     point, event_ms, ..
                 }| {
                    let duration = Duration::from_millis(prev_ms.abs_diff(event_ms));
                    let distance = Haversine.distance(*prev_p, point);

                    if duration <= args.gap && distance <= args.jump {
                        *prev_p = point;
                        *prev_ms = event_ms;
                        Some(point)
                    } else {
                        None
                    }
                },
            )
            .collect::<Vec<_>>();

        let history = std::iter::once(payload.point).chain(context).collect();

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
