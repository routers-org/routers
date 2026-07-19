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

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use geo::{Distance, Haversine, Point};
use log::{debug, info};
use tracing::{Instrument, field, info_span};
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
    jump: f64,
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

    let subscriber = client
        .subscribe(args.inbound_subject)
        .await
        .context("could not subscribe to NATS subject")?;
    let events = NATSStream::<Payload>::new(subscriber).map(Inbound::Event);

    let subscriber = client
        .subscribe(args.results_subject)
        .await
        .context("could not subscribe to results subject")?;
    let results = NATSStream::<MatchResult<E, M>>::new(subscriber).map(Inbound::Result);

    let mut source = futures::stream::select(events, results);

    let mut kv = RedisStore::<RawEvent>::new(args.redis)
        .await
        .context("could not connect to redis store")?;

    let gap = chrono::Duration::from_std(args.gap).context("gap out of range")?;

    // Each vehicle's trellis from its prior solve, as committed back by the
    // matcher's results. Derived state, not a source of truth: losing it
    // (restart, first sight) just means the next context says `Restart` and
    // the matcher rebuilds from the committed history.
    let mut trips: HashMap<String, Trip<E>> = HashMap::new();

    while let Some(inbound) = source.next().await {
        let payload = match inbound {
            Inbound::Event(payload) => payload,
            // Commit-action for a completed solve: the returned trip becomes
            // the state the vehicle's next context resumes from.
            Inbound::Result(result) => {
                let _span = info_span!("commit_result", layers = result.trip.layers()).entered();
                trips.insert(result.vehicle_id, result.trip);
                continue;
            }
        };

        // One span per event; the collector aggregates these into the
        // orchestrator's throughput and latency series. `continuation` is
        // recorded once reconciliation decides, and becomes a metric label.
        let span = info_span!(
            "orchestrate",
            continuation = field::Empty,
            fresh = field::Empty,
            cut = field::Empty,
        );

        async {
            let mut entries = kv
                .get_many(&payload.vehicle_id, args.context_window)
                .instrument(info_span!("context_fetch"))
                .await
                .context("could not get entries from redis store")?;

            // Normalise to newest-first regardless of the datasource's return
            // order: the cutoff below walks back in time from the current event,
            // discarding everything beyond the first gap or teleport.
            entries.sort_by_key(|event| std::cmp::Reverse(event.timestamp));
            let fetched = entries.len();

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

            // A cutoff means a gap or teleport discarded committed history —
            // a marker span makes the occurrences countable.
            let cut = fetched - context.len();
            if cut > 0 {
                info_span!("history_cut", reason = "gap_or_teleport").in_scope(|| {});
            }

            // The matcher solves a directed trajectory, so it must receive the
            // points in chronological order. The current payload may already be
            // archived, so dedup by timestamp after sorting.
            let mut history: Vec<RawEvent> =
                std::iter::once(payload.as_event()).chain(context).collect();
            history.sort_by_key(|event| event.timestamp);
            history.dedup_by_key(|event| event.timestamp);

            let points = history
                .into_iter()
                .map(|event| event.point)
                .collect::<Vec<Point>>();

            // Reconcile the prior solve against the committed window: pure data
            // work (trim and compare — never generating a layer), so it belongs
            // here rather than on the matcher's hot path. The trip is cloned,
            // not taken: a second event racing the first result still resumes
            // from the same state, just with one more fresh point.
            let continuation = info_span!("reconcile").in_scope(|| {
                Continuation::reconcile(trips.get(&payload.vehicle_id).cloned(), &points)
            });

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

            sink.send(MatchContext {
                vehicle_id: payload.vehicle_id,
                continuation,
            })
            .instrument(info_span!("publish_context"))
            .await
            .context("could not send match context")
        }
        .instrument(span)
        .await?;
    }

    sink.close().await?;

    Ok(())
}
