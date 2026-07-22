use std::collections::HashMap;
use std::hash::{DefaultHasher, Hash, Hasher};
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
use log::{debug, error, info};
use tokio::sync::mpsc;
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

/// An [`Inbound`] handed to a worker, tagged with the wall-clock stamps the
/// dispatch loop captured: when it was queued (for channel-residency timing)
/// and its wire send time (for end-to-end timing, and as the vehicle's origin).
struct Dispatch {
    queued_at: web_time::SystemTime,
    sent_at: Option<web_time::SystemTime>,
    inbound: Inbound,
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
    /// commits each result's trip as the vehicle's resume state.
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

    /// How many workers to fan vehicles across. Each vehicle is pinned to one
    /// by hash, so its events and results stay ordered on a worker that owns
    /// their trip state outright — the maps need no locks, and the per-event
    /// Redis fetch (the throughput bound) overlaps across vehicles.
    #[arg(short, env, long, default_value = "8")]
    workers: usize,
}

/// A worker's view of the shared configuration and its own trip state, borrowed
/// per event for [`try_create_context`](App::try_create_context).
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

    let events = NATSStream::<Payload>::new(
        client
            .subscribe(args.inbound_subject)
            .await
            .context("could not subscribe to NATS event subject")?,
    )
    .map(Inbound::Event);

    let results = NATSStream::<MatchResult<E, M>>::new(
        client
            .subscribe(args.results_subject)
            .await
            .context("could not subscribe to NATS results subject")?,
    )
    .map(Inbound::Result);

    let mut source = futures::stream::select(events, results);

    let gap = chrono::Duration::from_std(args.gap).context("gap out of range")?;

    let mut handles = Vec::with_capacity(args.workers);
    let mut txs = Vec::with_capacity(args.workers);

    for _ in 0..args.workers {
        let (tx, mut rx) = mpsc::channel::<Dispatch>(1024);
        txs.push(tx);

        let mut kv = RedisStore::<RawEvent>::new(args.redis.clone())
            .await
            .context("could not connect to redis store")?;

        let subject = args.outbound_subject.clone();
        let mut sink = NATSSink::<MatchContext<E>>::new(client.clone(), move |_| subject.clone());

        let context_window = args.context_window;
        let jump_distance = args.jump_distance;

        handles.push(tokio::spawn(async move {
            // This worker's vehicles, and their state. `trips` is the trellis
            // from each vehicle's last solve, committed back by the matcher and
            // resumed from on its next event; `origins` is when each vehicle's
            // newest event was published, for the end-to-end `event_to_match`
            // span. Both are derived — losing them just forces a restart.
            let mut trips: HashMap<String, Trip<E>> = HashMap::new();
            let mut origins: HashMap<String, web_time::SystemTime> = HashMap::new();

            while let Some(Dispatch {
                queued_at,
                sent_at,
                inbound,
            }) = rx.recv().await
            {
                routers_realtime::bus::span_between(
                    "worker_wait",
                    queued_at,
                    routers_realtime::bus::wallclock(),
                );

                match inbound {
                    Inbound::Event(payload) => {
                        if let Some(sent_at) = sent_at {
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
                            context_window,
                            jump_distance,
                            trips: &trips,
                            kv: &mut kv,
                        };

                        match app
                            .try_create_context(payload)
                            .instrument(span.clone())
                            .await
                        {
                            Ok(ctx) => {
                                if let Err(err) = sink
                                    .send(ctx)
                                    .instrument(info_span!(parent: &span, "publish_context"))
                                    .await
                                {
                                    error!("could not send match context: {err:#}");
                                }
                            }
                            Err(err) => warn!("could not create match context: {err}"),
                        }
                    }
                    Inbound::Result(result) => {
                        let _span =
                            info_span!("commit_result", layers = result.trip.layers()).entered();

                        if let (Some(origin), Some(matched_at)) =
                            (origins.remove(&result.vehicle_id), sent_at)
                        {
                            routers_realtime::bus::span_between(
                                "event_to_match",
                                origin,
                                matched_at,
                            );
                        }

                        trips.insert(result.vehicle_id, result.trip);
                    }
                }
            }
        }));
    }

    while let Some(inbound) = source.next().await {
        // The wire stamp is only valid while this message is the stream's newest
        // yield, so capture it here rather than in the worker.
        let sent_at = routers_realtime::bus::last_sent_at();

        let vehicle_id = match &inbound {
            Inbound::Event(payload) => &payload.vehicle_id,
            Inbound::Result(result) => &result.vehicle_id,
        };

        let mut hasher = DefaultHasher::new();
        vehicle_id.hash(&mut hasher);
        let worker = hasher.finish() as usize % args.workers;

        txs[worker]
            .send(Dispatch {
                queued_at: routers_realtime::bus::wallclock(),
                sent_at,
                inbound,
            })
            .await
            .map_err(|_| anyhow::anyhow!("worker {worker} channel closed"))?;
    }

    // Dropping the senders drains each worker before the process exits.
    drop(txs);
    for handle in handles {
        handle.await.ok();
    }

    Ok(())
}

impl App<'_> {
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
            .get_many(&vehicle_id, self.context_window * 3)
            .instrument(info_span!("context_fetch"))
            .await
            .context("could not get entries from redis store")?;

        entries.retain(|event| event.timestamp <= timestamp);
        entries.sort_by_key(|event| std::cmp::Reverse(event.timestamp));
        entries.truncate(self.context_window);

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
            point,
            timestamp,
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
            vehicle_id,
            continuation,
        })
    }
}
