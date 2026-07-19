use std::path::PathBuf;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::Metadata;
use routers_realtime::{
    bus::{NATSSink, NATSStream},
    event::{MatchContext, MatchResult},
};
use routers_shard::{FileFetcher, Geohash, ShardLoader};
use routers_transition::{
    Continuation, MatchError, Matcher, candidate::RoutedPath, costing::CostingStrategies,
    layer::generation::StandardGenerator, primitives::PredicateCache, weigh::AllCompute,
};

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use log::{debug, error, info, warn};
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
}

type E = OsmEntryId;
type M = OsmEdgeMetadata;

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

    let mut sink =
        NATSSink::<MatchResult<E, M>>::new(client.clone(), move |_| args.outbound_subject.clone());

    let subscriber = client
        .subscribe(args.inbound_subject)
        .await
        .context("could not subscribe to NATS subject")?;
    let mut source = NATSStream::<MatchContext<E>>::new(subscriber);

    // One long-lived matcher: the predicate cache and generator index stay
    // warm across every vehicle and event.
    let cache = Arc::new(PredicateCache::default());
    let runtime = OsmEdgeMetadata::runtime(None);
    let costing = CostingStrategies::default();

    let mut generator = StandardGenerator::new(network.as_ref(), &costing.emission);
    if let Some(distance) = args.search_distance {
        generator = generator.with_search_distance(distance);
    }

    let weigher = AllCompute::default().use_cache(cache);
    let matcher = Matcher::new(network.as_ref(), &costing, generator, weigher, &runtime);

    while let Some(MatchContext {
        vehicle_id,
        continuation,
    }) = source.next().await
    {
        // One span per event: `outcome`/`severity` become the success-ratio
        // labels, `continuation` splits every series by resume vs restart.
        let span = info_span!(
            "match_event",
            outcome = field::Empty,
            severity = field::Empty,
            continuation = field::Empty,
        );

        async {
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
                return Ok(());
            }

            // A snapshot is only defined over a solved trip: solve first, and
            // let a failure (e.g. a disconnected boundary) surface here, before
            // any collapse is attempted.
            if let Err(err) = info_span!("solve").in_scope(|| matcher.solve(&mut trip).map(|_| ()))
            {
                let (outcome, severity) = classify(&err);
                span.record("outcome", outcome);
                span.record("severity", severity);
                error!("{vehicle_id}: unable to solve trip: {err}");
                return Ok(());
            }

            match info_span!("snapshot").in_scope(|| matcher.snapshot(&mut trip)) {
                Ok(solution) => {
                    let path = RoutedPath::new(solution, network.as_ref());
                    span.record("outcome", "success");
                    span.record("severity", "ok");

                    sink.send(MatchResult {
                        path,
                        vehicle_id,
                        trip,
                    })
                    .instrument(info_span!("publish_result"))
                    .await
                    .context("could not emit result to sink")
                }
                Err(err) => {
                    let (outcome, severity) = classify(&err);
                    span.record("outcome", outcome);
                    span.record("severity", severity);
                    error!("{vehicle_id}: unable to match payload: {err}");
                    Ok(())
                }
            }
        }
        .instrument(span)
        .await?;
    }

    loop {
        sleep(Duration::from_secs(1));
    }
}
