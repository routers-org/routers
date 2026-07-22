use std::path::PathBuf;
use std::sync::Arc;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::Metadata;
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
use futures::StreamExt;
use log::{debug, error, info, warn};
use tracing::{field, info_span};
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

    /// How many contexts to solve concurrently. Solving is CPU-bound and each
    /// context is self-contained, so contexts fan out across a blocking pool
    /// with no shared state to serialise on.
    #[arg(short, env, long, default_value = "5")]
    workers: usize,
}

type E = OsmEntryId;
type M = OsmEdgeMetadata;
type Net = ShardedNetwork<E, M, Geohash>;

/// Everything a solve needs, owned so the service can be shared (`Arc`) across
/// the concurrent solves without leaking or juggling `'static` borrows. The
/// network's spatial index and the predicate cache are the only heavy state,
/// and both are shared; a per-solve [`Matcher`] is just a bundle of borrows
/// into this and is free to build.
struct Matching {
    network: Arc<Net>,
    runtime: <M as Metadata>::Runtime,
    costing: CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, E>,
    cache: Arc<PredicateCache<E, M, Net>>,
    search_distance: Option<f64>,
}

impl Matching {
    /// Solve one context, recording its outcome onto a fresh `match_event`
    /// span. Returns the result to publish, or `None` when there is nothing to
    /// emit (no anchor, or a nominal/fatal solve failure).
    fn solve(
        &self,
        MatchContext {
            vehicle_id,
            continuation,
        }: MatchContext<E>,
    ) -> Option<MatchResult<E, M>> {
        let mut generator = StandardGenerator::new(self.network.as_ref(), &self.costing.emission);
        if let Some(distance) = self.search_distance {
            generator = generator.with_search_distance(distance);
        }

        let weigher = AllCompute::default().use_cache(self.cache.clone());
        let matcher = Matcher::new(
            self.network.as_ref(),
            &self.costing,
            generator,
            weigher,
            &self.runtime,
        );

        let span = info_span!(
            "match_event",
            outcome = field::Empty,
            severity = field::Empty,
            continuation = field::Empty,
        );
        let _entered = span.enter();

        let (mut trip, fresh) = match continuation {
            Continuation::Resume { trip, fresh } => {
                span.record("continuation", "resume");
                (trip, fresh)
            }
            Continuation::Restart { fresh } => {
                span.record("continuation", "restart");
                (matcher.begin(), fresh)
            }
        };

        info_span!("push", points = fresh.len()).in_scope(|| {
            for point in fresh {
                match matcher.push(&mut trip, point) {
                    Ok(_) => {}
                    Err(MatchError::Unanchored(err)) => {
                        info_span!("point_drop", reason = "unanchored")
                            .in_scope(|| debug!("{vehicle_id}: dropped off-network point ({err})"));
                    }
                    Err(err) => {
                        info_span!("point_drop", reason = "push_error")
                            .in_scope(|| error!("{vehicle_id}: could not push point: {err}"));
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

        if let Err(err) = info_span!("solve").in_scope(|| matcher.solve(&mut trip)) {
            let (outcome, severity) = classify(err);
            span.record("outcome", outcome);
            span.record("severity", severity);
            error!("{vehicle_id}: unable to solve trip");
            return None;
        }

        let solution = match info_span!("snapshot").in_scope(|| matcher.snapshot(&mut trip)) {
            Ok(solution) => solution,
            Err(err) => {
                let (outcome, severity) = classify(err);
                span.record("outcome", outcome);
                span.record("severity", severity);
                return None;
            }
        };

        span.record("outcome", "success");
        span.record("severity", "ok");

        let path = RoutedPath::new(solution, self.network.as_ref());
        Some(MatchResult {
            path,
            vehicle_id,
            trip,
        })
    }
}

/// A match attempt's `outcome`/`severity` labels for the success-ratio series.
/// Nominal failures are the data's fault (a point off every road, a trace the
/// network cannot bridge) and expected in healthy operation; fatal ones are ours.
fn classify(err: MatchError) -> (&'static str, &'static str) {
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
    let source = NATSStream::<MatchContext<E>>::new(subscriber);

    let sink = NATSSink::<MatchResult<E, M>>::new(client, move |_| args.outbound_subject.clone());

    let matching = Arc::new(Matching {
        network,
        runtime: OsmEdgeMetadata::runtime(None),
        costing: CostingStrategies::default(),
        cache: Arc::new(PredicateCache::default()),
        search_distance: args.search_distance,
    });

    // Each context is solved on the blocking pool (solving is synchronous and
    // CPU-bound); `buffer_unordered` keeps `workers` in flight at once. Results
    // stream straight into the sink in completion order.
    source
        .map(|context| {
            let matching = Arc::clone(&matching);
            async move {
                tokio::task::spawn_blocking(move || matching.solve(context))
                    .await
                    .ok()
                    .flatten()
            }
        })
        .buffer_unordered(args.workers)
        .filter_map(std::future::ready)
        .map(Ok)
        .forward(sink)
        .await?;

    error!("source terminated");
    Ok(())
}
