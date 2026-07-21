use std::collections::HashMap;
use std::time::Duration;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_realtime::{
    bus::{NATSSink, NATSStream},
    event::{MatchContext, MatchResult, Payload, RawEvent},
    store::RedisStore,
};
use routers_transition::Continuation;
use routers_transition::matcher::Trip;

use anyhow::{Context, Result};
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use geo::{Distance, Haversine, Point};
use log::{debug, info};
use tracing::{Instrument, field, info_span, warn};
use url::Url;

type E = OsmEntryId;
type M = OsmEdgeMetadata;

/// Everything the orchestrator reacts to: raw positions to assemble context
/// for, and match results whose trip markers it commits to the store.
enum Inbound {
    Event(Payload),
    Result(MatchResult<E, M>),
}

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

    /// The NATS subject the matcher publishes results into. The orchestrator
    /// commits each result's trip as the vehicle's resume state — the
    /// matcher itself never touches a store.
    #[arg(long = "results", env)]
    results_subject: String,

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
    jump_distance: f64,
}

struct App<'a> {
    gap: chrono::TimeDelta,

    jump_distance: f64,
    context_window: usize,

    trips: &'a HashMap<String, Trip<E>>,

    kv: &'a mut RedisStore<RawEvent>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = routers_realtime::telemetry::init("routers-orchestrator");

    let args = Args::parse();
    info!("orchestrator started: {:?}", args);

    let nats_url = ServerAddr::from_url(args.nats).context("could not create NATS url")?;

    let client = ConnectOptions::new()
        .name("OrchestratorService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;

    let mut sink =
        NATSSink::<MatchContext<E>>::new(client.clone(), move |_| args.outbound_subject.clone());

    let events_subscriber = client
        .subscribe(args.inbound_subject)
        .await
        .context("could not subscribe to NATS event subject")?;

    let results_subscriber = client
        .subscribe(args.results_subject)
        .await
        .context("could not subscribe to NATS results subject")?;

    let events = NATSStream::<Payload>::new(events_subscriber).map(Inbound::Event);
    let results = NATSStream::<MatchResult<E, M>>::new(results_subscriber).map(Inbound::Result);

    let mut source = futures::stream::select(events, results);

    let mut kv = RedisStore::<RawEvent>::new(args.redis)
        .await
        .context("could not connect to redis store")?;

    let gap = chrono::Duration::from_std(args.gap).context("gap out of range")?;

    let mut trips: HashMap<String, Trip<E>> = HashMap::new();
    let mut origins: HashMap<String, web_time::SystemTime> = HashMap::new();

    while let Some(inbound) = source.next().await {
        match inbound {
            Inbound::Event(payload) => {
                if let Some(sent_at) = routers_realtime::bus::last_sent_at() {
                    origins.insert(payload.vehicle_id.clone(), sent_at);
                }

                let span = info_span!(
                    "orchestrate",
                    continuation = field::Empty,
                    fresh = field::Empty,
                    cut = field::Empty,
                );

                let mut app = App {
                    gap,
                    context_window: args.context_window,
                    jump_distance: args.jump_distance,
                    trips: &trips,
                    kv: &mut kv,
                };

                match app.try_create_context(payload).instrument(span.clone()).await {
                    Ok(ctx) => {
                        sink.send(ctx)
                            .instrument(info_span!(parent: &span, "publish_context"))
                            .await
                            .context("could not send match context")?;
                    }
                    Err(err) => {
                        warn!("could not create match context: {err}");
                    }
                }
            }
            Inbound::Result(result) => {
                let _span = info_span!("commit_result", layers = result.trip.layers()).entered();

                if let (Some(origin), Some(matched_at)) = (
                    origins.remove(&result.vehicle_id),
                    routers_realtime::bus::last_sent_at(),
                ) {
                    routers_realtime::bus::span_between("event_to_match", origin, matched_at);
                }

                trips.insert(result.vehicle_id, result.trip);
                continue;
            }
        };
    }

    sink.close().await?;

    Ok(())
}

impl<'a> App<'a> {
    async fn try_create_context(
        &mut self,
        Payload {
            vehicle_id,
            timestamp,
            point,
            ..
        }: Payload,
    ) -> Result<MatchContext<E>> {
        let mut entries = self
            .kv
            .get_many(&vehicle_id, self.context_window)
            .instrument(info_span!("context_fetch"))
            .await
            .context("could not get entries from redis store")?;

        entries.sort_by_key(|event| std::cmp::Reverse(event.timestamp));
        let fetched = entries.len();

        let context = entries
            .into_iter()
            .inspect(|v| debug!("event: {:?}", v))
            .scan((point, timestamp), |(prev_p, prev_ts), event: RawEvent| {
                let duration = (*prev_ts - event.timestamp).abs();
                let distance = Haversine.distance(*prev_p, event.point);

                if duration <= self.gap && distance <= self.jump_distance {
                    *prev_p = event.point;
                    *prev_ts = event.timestamp;
                    Some(event)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();

        let cut = fetched - context.len();
        if cut > 0 {
            info_span!("history_cut", reason = "gap_or_teleport").in_scope(|| {});
        }

        let mut history: Vec<RawEvent> = std::iter::once(RawEvent {
            vehicle_id: vehicle_id.clone(),
            point: point,
            timestamp: timestamp,
        })
        .chain(context)
        .collect();

        history.sort_by_key(|event| event.timestamp);
        history.dedup_by_key(|event| event.timestamp);

        let points = history
            .into_iter()
            .map(|event| event.point)
            .collect::<Vec<Point>>();

        let continuation = info_span!("reconcile")
            .in_scope(|| Continuation::reconcile(self.trips.get(&vehicle_id).cloned(), &points));

        let span = tracing::Span::current();
        span.record("cut", cut);
        match &continuation {
            Continuation::Resume { fresh, .. } => {
                span.record("continuation", "resume");
                span.record("fresh", fresh.len());
            }
            Continuation::Restart { fresh } => {
                span.record("continuation", "restart");
                span.record("fresh", fresh.len());
            }
        }

        Ok(MatchContext {
            vehicle_id: vehicle_id,
            continuation,
        })
    }
}
