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

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

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
        // The orchestrator reconciled but cannot generate a layer, so both
        // cases land here with points still to push: `Resume` hands back the
        // trellis from the prior solve, `Restart` means no prior solve
        // stands (first point, or the history diverged) and we begin anew.
        let (mut trip, fresh) = match continuation {
            Continuation::Resume { trip, fresh } => {
                debug!(
                    "{vehicle_id}: resuming {} committed layer(s), {} fresh point(s)",
                    trip.layers(),
                    fresh.len()
                );
                (trip, fresh)
            }
            Continuation::Restart { fresh } => {
                debug!("{vehicle_id}: restarting over {} point(s)", fresh.len());
                (matcher.begin(), fresh)
            }
        };

        for point in fresh {
            match matcher.push(&mut trip, point) {
                Ok(_) => {}
                Err(MatchError::Unanchored(err)) => {
                    debug!("{vehicle_id}: dropped off-network point ({err})");
                }
                Err(err) => {
                    error!("{vehicle_id}: could not push point: {err}");
                }
            }
        }

        // Only a trip with at least one anchored layer is solvable; every
        // fresh point rejecting (all off-network) leaves nothing to do.
        if trip.is_empty() {
            warn!("{vehicle_id}: no anchored layers to solve");
            continue;
        }

        // A snapshot is only defined over a solved trip: solve first, and
        // let a failure (e.g. a disconnected boundary) surface here, before
        // any collapse is attempted.
        if let Err(err) = matcher.solve(&mut trip) {
            error!("{vehicle_id}: unable to solve trip: {err}");
            continue;
        }

        match matcher.snapshot(&mut trip) {
            Ok(solution) => {
                let path = RoutedPath::new(solution, network.as_ref());

                sink.send(MatchResult {
                    path,
                    vehicle_id,
                    trip,
                })
                .await
                .context("could not emit result to sink")?;
            }
            Err(err) => {
                error!("{vehicle_id}: unable to match payload: {err}");
            }
        }
    }

    loop {
        sleep(Duration::from_secs(1));
    }
}
