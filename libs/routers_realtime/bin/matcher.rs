use std::hash::{Hash, Hasher};
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
use std::{hash::DefaultHasher, path::PathBuf};

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::{Metadata, Network};
use routers_realtime::{
    bus::{NATSSink, NATSStream},
    event::{MatchContext, MatchResult},
};
use routers_shard::{FileFetcher, Geohash, ShardLoader, ShardedNetwork};
use routers_transition::{
    Continuation, MatchError, Matcher,
    candidate::RoutedPath,
    costing::{CostingStrategies, DefaultEmissionCost, DefaultTransitionCost},
    layer::generation::StandardGenerator,
    primitives::PredicateCache,
    weigh::AllCompute,
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

    /// The number of worker threads to use for matching
    #[arg(short, env, long, default_value = "5")]
    workers: usize,

    // The inbound NATS subject to subscribe to, for matching events.
    #[arg(short, env, long)]
    inbound_subject: String,

    // The outbound NATS subject to publish matching results to.
    #[arg(short, env, long)]
    outbound_subject: String,

    /// The search distance to use for matching
    #[arg(long, env)]
    search_distance: Option<f64>,

    /// The most candidates a layer may hold (the k cheapest by emission).
    /// Bounds boundary weighing at k² pairs regardless of road density, so
    /// solve cost stays flat through dense grids without shrinking the
    /// search radius (which costs anchoring instead).
    #[arg(long, env)]
    max_candidates: Option<usize>,

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
type Match<'a> = Matcher<
    'a,
    DefaultEmissionCost,
    DefaultTransitionCost,
    StandardGenerator<'a, E, M, DefaultEmissionCost>,
    AllCompute<E, M, ShardedNetwork<E, M, Geohash>>,
    E,
    M,
    ShardedNetwork<E, M, Geohash>,
>;

/// How a match attempt ended, as the `outcome`/`severity` labels the
/// collector turns into the success-ratio series. Nominal failures are the
/// data's fault (a point off every road, a trace the network cannot bridge)
/// and are expected in healthy operation; fatal ones are ours.
fn classify(err: &MatchError) -> (&'static str, &'static str) {
    match err {
        MatchError::Unanchored(_) => ("unanchored", "nominal"),
        MatchError::Disconnected(_) => ("disconnected", "nominal"),
        MatchError::TrellisError(_) | MatchError::SolveError(_) => ("internal", "fatal"),
    }
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

    // One long-lived matcher, shared by every worker: the predicate cache
    // and generator index stay warm across every vehicle and event. The
    // matcher and its dependencies live for the life of the process, so we
    // leak them once to get the `'static` borrows `tokio::spawn` requires.
    let network: &'static Arc<ShardedNetwork<E, M, Geohash>> = Box::leak(Box::new(network));
    let net: &'static ShardedNetwork<E, M, Geohash> = network;
    let runtime: &'static _ = Box::leak(Box::new(OsmEdgeMetadata::runtime(None)));
    let costing: &'static CostingStrategies<_, _, E> =
        Box::leak(Box::new(CostingStrategies::default()));

    let cache = Arc::new(PredicateCache::default());

    let mut generator = StandardGenerator::new(net, &costing.emission);
    if let Some(distance) = args.search_distance {
        generator = generator.with_search_distance(distance);
    }
    if let Some(k) = args.max_candidates {
        generator = generator.with_max_candidates(k);
    }

    let weigher = AllCompute::default().use_cache(cache);
    let matcher: &'static Match<'static> = Box::leak(Box::new(Matcher::new(
        net, costing, generator, weigher, runtime,
    )));

    let txs: Vec<_> = (0..args.workers)
        .map(|_| {
            // Stamped on dispatch so the channel residency below is
            // measurable: `queue_wait` ends when the NATS stream yields, and
            // `match_event` starts in the worker — this covers the hop
            // between them, where a busy worker queues its vehicles. The
            // second stamp is the context's wire send time (captured at
            // dispatch, while the message is the stream's newest yield),
            // which is what the shed deadline is measured against.
            let (tx, mut rx) = mpsc::channel::<(
                web_time::SystemTime,
                Option<web_time::SystemTime>,
                MatchContext<E>,
            )>(1024);

            // Each worker publishes through its own sink; the underlying
            // NATS client is shared and cheap to clone.
            let subject = args.outbound_subject.clone();
            let mut sink =
                NATSSink::<MatchResult<E, M>>::new(client.clone(), move |_| subject.clone());

            let shed_after = args.shed_after;
            tokio::spawn(async move {
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

                    // One span per event: `outcome`/`severity` become the
                    // success-ratio labels, `continuation` splits every
                    // series by resume vs restart. `match_event` records
                    // into it via `Span::current()`.
                    let span = info_span!(
                        "match_event",
                        outcome = field::Empty,
                        severity = field::Empty,
                        continuation = field::Empty,
                    );

                    async {
                        if let Some(result) = match_event(matcher, net, msg).await {
                            sink.send(result)
                                .instrument(info_span!("publish_result"))
                                .await
                                .context("could not emit result to sink")
                                .ok();
                        }
                    }
                    .instrument(span)
                    .await;
                }
            });

            tx
        })
        .collect();

    while let Some(msg) = source.next().await {
        // Only valid while this message is the stream's newest yield.
        let sent_at = routers_realtime::bus::last_sent_at();

        let mut h = DefaultHasher::new();
        msg.vehicle_id.hash(&mut h);

        txs[h.finish() as usize % args.workers]
            .send((routers_realtime::bus::wallclock(), sent_at, msg))
            .await
            .unwrap();
    }

    error!("source terminated");

    loop {
        sleep(Duration::from_secs(1));
    }
}

async fn match_event<'a>(
    matcher: &Match<'a>,
    network: &impl Network<E, M>,
    MatchContext {
        vehicle_id,
        continuation,
    }: MatchContext<E>,
) -> Option<MatchResult<E, M>> {
    let span = tracing::Span::current();

    // The orchestrator reconciled but cannot generate a layer, so both
    // cases land here with points still to push: `Resume` hands back the
    // trellis from the prior solve, `Restart` means no prior solve
    // stands (first point, or the history diverged) and we begin anew.
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
                Ok(_) => {}
                Err(MatchError::Unanchored(err)) => {
                    // Zero-duration marker: the collector counts it.
                    info_span!("point_drop", reason = "unanchored").in_scope(|| {});
                    debug!("{vehicle_id}: dropped off-network point ({err})");
                }
                Err(err) => {
                    info_span!("point_drop", reason = "push_error").in_scope(|| {});
                    error!("{vehicle_id}: could not push point: {err}");
                }
            }
        }
    });

    // Only a trip with at least one anchored layer is solvable; every
    // fresh point rejecting (all off-network) leaves nothing to do.
    if trip.is_empty() {
        span.record("outcome", "no_anchor");
        span.record("severity", "nominal");
        warn!("{vehicle_id}: no anchored layers to solve");
        return None;
    }

    // A snapshot is only defined over a solved trip: solve first, and
    // let a failure (e.g. a disconnected boundary) surface here, before
    // any collapse is attempted.
    if let Err(err) = info_span!("solve").in_scope(|| matcher.solve(&mut trip).map(|_| ())) {
        let (outcome, severity) = classify(&err);
        span.record("outcome", outcome);
        span.record("severity", severity);
        error!("{vehicle_id}: unable to solve trip: {err}");
        return None;
    }

    match info_span!("snapshot").in_scope(|| matcher.snapshot(&mut trip)) {
        Ok(solution) => {
            let path = RoutedPath::new(solution, network);
            span.record("outcome", "success");
            span.record("severity", "ok");

            Some(MatchResult {
                path,
                vehicle_id,
                trip,
            })
        }
        Err(err) => {
            let (outcome, severity) = classify(&err);
            span.record("outcome", outcome);
            span.record("severity", severity);
            error!("{vehicle_id}: unable to match payload: {err}");
            None
        }
    }
}
