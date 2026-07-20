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

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use geo::{Distance, Haversine, Point};
use log::{debug, error, info};
use tokio::sync::mpsc;
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

    /// The number of concurrent workers. Each vehicle is pinned to one by
    /// hash, so its events and results stay strictly ordered while the
    /// per-event Redis fetches overlap across vehicles — the fetch round
    /// trip, not compute, is what bounds a single orchestrator's throughput.
    #[arg(short, env, long, default_value = "8")]
    workers: usize,

    /// The most fresh points a context may carry; a longer tail (a restart
    /// over a wide window, or a resume across a long commit gap) is
    /// restarted over just the newest points instead. Matcher work per
    /// event is ~one boundary per fresh point, so this is the per-event
    /// work budget — it decouples the resume overlap horizon (wide, cheap:
    /// `--context-window`) from the matching cost (bounded, regardless of
    /// how turbulent the pipeline has been).
    #[arg(long, env, default_value = "8")]
    fresh_cap: usize,
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

    let gap = chrono::Duration::from_std(args.gap).context("gap out of range")?;

    // Vehicles are pinned to workers by hash: the per-event Redis fetches
    // overlap across vehicles while each vehicle's events — and the results
    // that commit its trips — stay strictly ordered on one worker. The trip
    // and origin maps shard along with the vehicles, so a worker owns every
    // vehicle it will ever see and the maps need no locks. Keeping a result
    // on its vehicle's worker is also what protects the resume ratio: the
    // commit lands before that vehicle's next event is reconciled.
    let mut handles = Vec::with_capacity(args.workers);
    let mut txs = Vec::with_capacity(args.workers);

    for _ in 0..args.workers {
        let (tx, mut rx) = mpsc::channel::<(
            web_time::SystemTime,
            Option<web_time::SystemTime>,
            Inbound,
        )>(1024);
        txs.push(tx);

        let mut kv = RedisStore::<RawEvent>::new(args.redis.clone())
            .await
            .context("could not connect to redis store")?;

        let subject = args.outbound_subject.clone();
        let mut sink = NATSSink::<MatchContext<E>>::new(client.clone(), move |_| subject.clone());

        let context_window = args.context_window;
        let jump = args.jump;
        let fresh_cap = args.fresh_cap;

        handles.push(tokio::spawn(async move {
            // Each vehicle's trellis from its prior solve, as committed back
            // by the matcher's results. Derived state, not a source of truth:
            // losing it (restart, first sight) just means the next context
            // says `Restart` and the matcher rebuilds from committed history.
            let mut trips: HashMap<String, Trip<E>> = HashMap::new();

            // When each vehicle's newest raw event was published (its wire
            // stamp), so the matching result can be measured against it: the
            // `event_to_match` span below is the pipeline's end-to-end
            // walltime, replay's publish → the matcher's publish, taken
            // entirely from message stamps.
            let mut origins: HashMap<String, web_time::SystemTime> = HashMap::new();

            while let Some((queued_at, sent_at, inbound)) = rx.recv().await {
                routers_realtime::bus::span_between(
                    "worker_wait",
                    queued_at,
                    routers_realtime::bus::wallclock(),
                );

                let payload = match inbound {
                    Inbound::Event(payload) => {
                        if let Some(sent_at) = sent_at {
                            origins.insert(payload.vehicle_id.clone(), sent_at);
                        }
                        payload
                    }
                    // Commit-action for a completed solve: the returned trip
                    // becomes the state the vehicle's next context resumes
                    // from.
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
                        continue;
                    }
                };

                // One span per event; the collector aggregates these into the
                // orchestrator's throughput and latency series. `continuation`
                // is recorded once reconciliation decides, and becomes a
                // metric label.
                let span = info_span!(
                    "orchestrate",
                    continuation = field::Empty,
                    fresh = field::Empty,
                    cut = field::Empty,
                );

                let orchestrated: anyhow::Result<()> = async {
                    // Fetched deeper than the window on purpose: the store may
                    // hold points *newer* than this event (see below), which
                    // are dropped before the window is taken.
                    let mut entries = kv
                        .get_many(&payload.vehicle_id, context_window * 3)
                        .instrument(info_span!("context_fetch"))
                        .await
                        .context("could not get entries from redis store")?;

                    // Context is strictly the event's past. The historian
                    // archives straight off the raw stream, so under any
                    // pipeline backlog the store runs *ahead* of this worker
                    // and the newest entries are the vehicle's future relative
                    // to the event being matched. Left in, the gap/teleport
                    // cutoffs below fire on pipeline latency rather than data
                    // quality (at replay speed s, one wall-second of backlog
                    // is s seconds — kilometres — of vehicle movement), which
                    // discards committed history and forces the matcher onto
                    // its expensive restart path: a feedback loop that
                    // collapses throughput under exactly the load that caused
                    // the backlog.
                    entries.retain(|event| event.timestamp <= payload.timestamp);

                    // Normalise to newest-first regardless of the
                    // datasource's return order: the cutoff below walks back
                    // in time from the current event, discarding everything
                    // beyond the first gap or teleport.
                    entries.sort_by_key(|event| std::cmp::Reverse(event.timestamp));
                    entries.truncate(context_window);
                    let fetched = entries.len();

                    let context = entries
                        .into_iter()
                        .inspect(|v| debug!("event: {:?}", v))
                        .scan(
                            (payload.point, payload.timestamp),
                            |(prev_p, prev_ts), event: RawEvent| {
                                let duration = (*prev_ts - event.timestamp).abs();
                                let distance = Haversine.distance(*prev_p, event.point);

                                if duration <= gap && distance <= jump {
                                    *prev_p = event.point;
                                    *prev_ts = event.timestamp;
                                    Some(event)
                                } else {
                                    None
                                }
                            },
                        )
                        .collect::<Vec<_>>();

                    // A cutoff means a gap or teleport discarded committed
                    // history — a marker span makes the occurrences countable.
                    let cut = fetched - context.len();
                    if cut > 0 {
                        info_span!("history_cut", reason = "gap_or_teleport").in_scope(|| {});
                    }

                    // The matcher solves a directed trajectory, so it must
                    // receive the points in chronological order. The current
                    // payload may already be archived, so dedup by timestamp
                    // after sorting.
                    let mut history: Vec<RawEvent> =
                        std::iter::once(payload.as_event()).chain(context).collect();
                    history.sort_by_key(|event| event.timestamp);
                    history.dedup_by_key(|event| event.timestamp);

                    let points = history
                        .into_iter()
                        .map(|event| event.point)
                        .collect::<Vec<Point>>();

                    // Reconcile the prior solve against the committed window:
                    // pure data work (trim and compare — never generating a
                    // layer), so it belongs here rather than on the matcher's
                    // hot path. The trip is cloned, not taken: a second event
                    // racing the first result still resumes from the same
                    // state, just with one more fresh point.
                    let continuation = info_span!("reconcile").in_scope(|| {
                        Continuation::reconcile(trips.get(&payload.vehicle_id).cloned(), &points)
                    });

                    // Enforce the per-event work budget: a context whose
                    // fresh tail exceeds the cap costs the matcher one
                    // boundary per point whether it resumes or restarts, so
                    // ship the cheapest equivalent — a restart over just the
                    // newest points. Marker span so the conversions are
                    // countable.
                    let continuation = match continuation {
                        Continuation::Resume { fresh, .. } | Continuation::Restart { fresh }
                            if fresh.len() > fresh_cap =>
                        {
                            info_span!("fresh_capped", reason = "over_budget").in_scope(|| {});
                            Continuation::Restart {
                                fresh: fresh[fresh.len() - fresh_cap..].to_vec(),
                            }
                        }
                        continuation => continuation,
                    };

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
                .await;

                // A failed event is logged and dropped rather than fatal: one
                // bad fetch must not take down every vehicle on this worker.
                if let Err(err) = orchestrated {
                    error!("orchestration failed: {err:#}");
                }
            }
        }));
    }

    // Accounts for the loop's time *between* iterations: awaiting the bus,
    // which includes wire decode and stream polling. Against `queue_wait` it
    // localises a backlog — waits growing while `recv_idle` stays near zero
    // mean this loop is the bottleneck; waits growing while the loop sits
    // mostly idle mean the messages are stuck upstream of it.
    let mut idle_from = routers_realtime::bus::wallclock();

    while let Some(inbound) = source.next().await {
        routers_realtime::bus::span_between(
            "recv_idle",
            idle_from,
            routers_realtime::bus::wallclock(),
        );

        // The wire stamp is an ambient slot valid only while this message is
        // the stream's most recent yield, so it must be captured here — by
        // the time a worker sees the message, later yields have overwritten
        // it. For an event it becomes the vehicle's origin; for a result it
        // is when the matcher published it.
        let sent_at = routers_realtime::bus::last_sent_at();

        let worker = {
            let vehicle_id = match &inbound {
                Inbound::Event(payload) => &payload.vehicle_id,
                Inbound::Result(result) => &result.vehicle_id,
            };

            let mut h = DefaultHasher::new();
            vehicle_id.hash(&mut h);
            h.finish() as usize % args.workers
        };

        txs[worker]
            .send((routers_realtime::bus::wallclock(), sent_at, inbound))
            .await
            .map_err(|_| anyhow::anyhow!("worker {worker} channel closed"))?;

        idle_from = routers_realtime::bus::wallclock();
    }

    // The stream has ended: dropping the senders lets each worker drain its
    // queue and flush its sink before the process exits.
    drop(txs);
    for handle in handles {
        handle.await.ok();
    }

    Ok(())
}
