use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::Metadata;
use routers_realtime::{
    bus::{NATSSink, NATSStream},
    event::{MatchContext, MatchResult},
};
use routers_shard::{FileFetcher, Geohash, ShardLoader, ShardedNetwork};
use routers_transition::{
    Continuation, MatchError, Matcher, candidate::RoutedPath, costing::CostingStrategies,
    layer::generation::StandardGenerator, primitives::PredicateCache, weigh::AllCompute,
};

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
use tokio::sync::mpsc;
use tracing::{Instrument, field, info_span};
use url::Url;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server
    #[arg(short, env, long)]
    nats: Url,

    /// The directory of stored shard files
    #[arg(short, env, long)]
    directory: PathBuf,

    /// The shard precision the system is configured to
    #[arg(short, env, long)]
    precision: usize,

    // The configured "owned" shard.
    #[arg(short, env, long)]
    shard: Geohash,

    // The inbound NATS subject to subscribe to, for matching events.
    #[arg(short, env, long)]
    inbound_subject: String,

    // The outbound NATS subject to publish matching results to.
    #[arg(short, env, long)]
    outbound_subject: String,

    /// The search distance to use for matching
    #[arg(long, env)]
    search_distance: Option<f64>,

    /// The number of concurrent workers. Each vehicle is pinned to one by
    /// hash, so its contexts stay strictly ordered while the solves overlap
    /// across vehicles. The matcher and its warm caches are shared by every
    /// worker; only the per-context solve runs in parallel.
    #[arg(short, env, long, default_value = "5")]
    workers: usize,

    /// Contexts older than this (wire send → pickup) are shed unmatched.
    /// A stale context is worthless: matching it commits an outdated trip,
    /// and the vehicle's next context is self-contained (window + trellis),
    /// so nothing is lost by skipping — while matching it anyway is what
    /// turns a transient backlog into a restart storm (stale commits break
    /// the orchestrator's resume overlap, and restarts cost ~3× a resume).
    /// In steady state nothing is old enough to shed; under overload the
    /// queue drains at drop speed instead of compounding.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "1s")]
    shed_after: Duration,
}

type E = OsmEntryId;
type M = OsmEdgeMetadata;
type Net = ShardedNetwork<E, M, Geohash>;
type Match<'a> = Matcher<
    'a,
    routers_transition::costing::DefaultEmissionCost,
    routers_transition::costing::DefaultTransitionCost,
    StandardGenerator<'a, E, M, routers_transition::costing::DefaultEmissionCost>,
    AllCompute<E, M, Net>,
    E,
    M,
    Net,
>;

/// How a match attempt ended, as the `outcome`/`severity` labels the
/// collector turns into the success-ratio series. Nominal failures are the
/// data's fault (a point off every road, a trace the network cannot bridge)
/// and are expected in healthy operation; fatal ones are ours.
fn classify(err: MatchError) -> (&'static str, &'static str) {
    match err {
        MatchError::Unanchored(_) => ("unanchored", "nominal"),
        MatchError::Disconnected(_) => ("disconnected", "nominal"),
        MatchError::TrellisError(_) | MatchError::SolveError(_) => ("internal", "fatal"),
    }
}

/// Run one context to completion, recording its outcome onto the current
/// `match_event` span. Returns the result to publish, or `None` when there
/// is nothing to emit (no anchor, or a nominal/fatal solve failure).
///
/// A free function rather than a closure so every worker can call the one
/// shared, warm matcher (leaked to `'static`) without cloning it.
fn attempt(
    matcher: &Match<'static>,
    network: &Net,
    vehicle_id: String,
    continuation: Continuation<E>,
) -> Option<MatchResult<E, M>> {
    let span = tracing::Span::current();

    let (mut trip, fresh) = match continuation {
        Continuation::Resume { trip, fresh } => {
            span.record("continuation", "resume");
            debug!(
                "{vehicle_id}: resuming {} committed layer(s), {} fresh point(s)",
                trip.layers(),
                fresh.len()
            );
            (trip, fresh)
        }
        Continuation::Restart { fresh } => {
            span.record("continuation", "restart");
            debug!("{vehicle_id}: restarting over {} point(s)", fresh.len());
            (matcher.begin(), fresh)
        }
    };

    info_span!("push", points = fresh.len()).in_scope(|| {
        for point in fresh {
            match matcher.push(&mut trip, point) {
                Ok(_) => { /* OK! */ }
                Err(MatchError::Unanchored(err)) => {
                    info_span!("point_drop", reason = "unanchored").in_scope(|| {
                        debug!("{vehicle_id}: dropped off-network point ({err})");
                    });
                }
                Err(err) => {
                    info_span!("point_drop", reason = "push_error").in_scope(|| {
                        error!("{vehicle_id}: could not push point: {err}");
                    });
                }
            }
        }
    });

    if trip.is_empty() {
        span.record("outcome", "no_anchor");
        span.record("severity", "nominal");
        warn!("{vehicle_id}: no anchored layers to solve");
        return None;
    }

    match info_span!("solve")
        .in_scope(|| matcher.solve(&mut trip))
        .map_err(classify)
    {
        Ok(path) => debug!("solved path: {path:?}"),
        Err((outcome, severity)) => {
            span.record("outcome", outcome);
            span.record("severity", severity);
            error!("{vehicle_id}: unable to solve trip: {outcome}");
            return None;
        }
    }

    let solution = match info_span!("snapshot")
        .in_scope(|| matcher.snapshot(&mut trip))
        .map_err(classify)
    {
        Ok(solution) => solution,
        Err((outcome, severity)) => {
            span.record("outcome", outcome);
            span.record("severity", severity);
            return None;
        }
    };

    span.record("outcome", "success");
    span.record("severity", "ok");

    let path = RoutedPath::new(solution, network);
    Some(MatchResult {
        path,
        vehicle_id,
        trip,
    })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = routers_realtime::telemetry::init("routers-matcher");

    let args = Args::parse();
    info!("matcher started: {:?}", args);

    let fetcher = FileFetcher::new(args.directory);
    let mut loader = ShardLoader::<E, M, Geohash, _, _>::new(fetcher, |key: &Geohash| {
        format!("{}.shard.rt", key)
    });

    let network = loader
        .load(&args.shard)
        .await
        .context("could not find shard in cache")?;

    let nats_url = ServerAddr::from_url(args.nats).context("could not create NATS url")?;

    let client = ConnectOptions::new()
        .name("MatcherService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;

    let subscriber = client
        .subscribe(args.inbound_subject)
        .await
        .context("could not subscribe to NATS subject")?;
    let mut source = NATSStream::<MatchContext<E>>::new(subscriber);

    // One long-lived matcher, shared by every worker: the predicate cache and
    // generator index stay warm across every vehicle and event. The matcher
    // and its dependencies live for the life of the process, so we leak them
    // once to get the `'static` borrows `tokio::spawn` requires.
    let network: &'static Arc<Net> = Box::leak(Box::new(network));
    let net: &'static Net = network.as_ref();
    let runtime: &'static _ = Box::leak(Box::new(OsmEdgeMetadata::runtime(None)));
    let costing: &'static CostingStrategies<_, _, E> =
        Box::leak(Box::new(CostingStrategies::default()));

    let cache = Arc::new(PredicateCache::default());

    let mut generator = StandardGenerator::new(net, &costing.emission);
    if let Some(distance) = args.search_distance {
        generator = generator.with_search_distance(distance);
    }

    let weigher = AllCompute::default().use_cache(cache);
    let matcher: &'static Match<'static> = Box::leak(Box::new(Matcher::new(
        net, costing, generator, weigher, runtime,
    )));

    // Vehicles are pinned to workers by hash: solves overlap across vehicles
    // while each vehicle's contexts stay strictly ordered on one worker.
    let mut handles = Vec::with_capacity(args.workers);
    let mut txs = Vec::with_capacity(args.workers);

    for _ in 0..args.workers {
        // Stamped on dispatch so the channel residency is measurable, and the
        // context's wire send time (captured while the message is the stream's
        // newest yield) rides along so the shed deadline can be checked here.
        let (tx, mut rx) = mpsc::channel::<(
            web_time::SystemTime,
            Option<web_time::SystemTime>,
            MatchContext<E>,
        )>(1024);
        txs.push(tx);

        // Each worker publishes through its own sink; the underlying NATS
        // client is shared and cheap to clone.
        let subject = args.outbound_subject.clone();
        let mut sink =
            NATSSink::<MatchResult<E, M>>::new(client.clone(), move |_| subject.clone());

        let shed_after = args.shed_after;
        handles.push(tokio::spawn(async move {
            while let Some((queued_at, sent_at, msg)) = rx.recv().await {
                routers_realtime::bus::span_between(
                    "worker_wait",
                    queued_at,
                    routers_realtime::bus::wallclock(),
                );

                // Past its deadline: shed rather than match (see
                // `--shed-after`). Zero-duration marker so the collector
                // counts the sheds.
                if let Some(sent_at) = sent_at
                    && routers_realtime::bus::wallclock()
                        .duration_since(sent_at)
                        .is_ok_and(|age| age > shed_after)
                {
                    info_span!("match_shed", reason = "stale").in_scope(|| {});
                    debug!("{}: shed stale context", msg.vehicle_id);
                    continue;
                }

                let MatchContext {
                    vehicle_id,
                    continuation,
                } = msg;

                let span = info_span!(
                    "match_event",
                    outcome = field::Empty,
                    severity = field::Empty,
                    continuation = field::Empty,
                );

                if let Some(result) =
                    span.in_scope(|| attempt(matcher, net, vehicle_id, continuation))
                {
                    sink.send(result)
                        .instrument(info_span!(parent: &span, "publish_result"))
                        .await
                        .context("could not emit result to sink")
                        .ok();
                }
            }
        }));
    }

    while let Some(msg) = source.next().await {
        // Only valid while this message is the stream's newest yield.
        let sent_at = routers_realtime::bus::last_sent_at();

        let mut h = DefaultHasher::new();
        msg.vehicle_id.hash(&mut h);
        let worker = h.finish() as usize % args.workers;

        txs[worker]
            .send((routers_realtime::bus::wallclock(), sent_at, msg))
            .await
            .map_err(|_| anyhow::anyhow!("worker {worker} channel closed"))?;
    }

    error!("source terminated");

    // Dropping the senders lets each worker drain its queue and flush its
    // sink before the process exits.
    drop(txs);
    for handle in handles {
        handle.await.ok();
    }

    Ok(())
}
