use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::{SinkExt, StreamExt};
use log::{error, info};
use routers::r#match::MatchOptions;
use routers::{Match, PredicateCache, SolverVariant};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::Metadata;
use routers_realtime::bus::{NATSSink, NATSStream};
use routers_realtime::event::{MatchContext, MatchResult};
use routers_shard::{FileFetcher, Geohash, ShardLoader};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread::sleep;
use std::time::Duration;
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
    #[arg(short, env, long)]
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
        .name("OrchestratorService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;

    let mut sink =
        NATSSink::<MatchResult<E, M>>::new(client.clone(), move |_| args.outbound_subject.clone());

    let subscriber = client
        .subscribe(args.inbound_subject)
        .await
        .context("could not subscribe to NATS subject")?;
    let mut source = NATSStream::<MatchContext>::new(subscriber);

    let cache = Arc::new(PredicateCache::<E, M, _>::default());
    let runtime = OsmEdgeMetadata::runtime(None);

    while let Some(MatchContext {
        history,
        vehicle_id,
    }) = source.next().await
    {
        let opts = MatchOptions::new()
            .with_runtime(runtime.clone())
            .with_cache(cache.clone())
            .with_solver(SolverVariant::Fastest)
            .with_search_distance(args.search_distance);

        match network.r#match(history.into(), opts) {
            Ok(path) => {
                sink.send(MatchResult { path, vehicle_id })
                    .await
                    .context("could not emit result to sink")?;
            }
            Err(err) => {
                error!("unable to match payload: {err}");
            }
        }
    }

    loop {
        sleep(Duration::from_secs(1));
    }
}
