use std::str::FromStr;
use std::sync::Arc;

use connectrpc::{Router, Server};
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_rpc::services::RPCAdapter;
use routers_shard::{
    FileFetcher, Geohash, GeohashStrategy, MultiShardNetwork, Selection, SelectionMode,
    ShardLoader, ShardingStrategy,
};
use schema::connect::routers::api::r#match::v1::MatchServiceExt;
use schema::connect::routers::api::optimise::v1::OptimiseServiceExt;
use schema::connect::routers::api::scan::v1::ScanServiceExt;

type E = OsmEntryId;
type M = OsmEdgeMetadata;
type S = Geohash;

fn shard_filename(key: &Geohash) -> String {
    format!("{}.shard.rt", key)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    routers_rpc::Tracer::init();

    let shard_dir = std::env::var("SHARD_DIR").unwrap_or_else(|_| "./shards".into());
    let shard_id_str = std::env::var("SHARD_ID").expect("SHARD_ID must be set");
    let shard_precision: u8 = std::env::var("SHARD_PRECISION")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5);
    let addr: std::net::SocketAddr = std::env::var("RPC_ADDR")
        .unwrap_or_else(|_| "[::1]:9001".into())
        .parse()?;

    let strategy = GeohashStrategy::with_precision(shard_precision);
    let shard = S::from_str(&shard_id_str)?;
    let selection = Selection::new(&strategy, shard, SelectionMode::OwnedAndNeighbours);

    let fetcher = FileFetcher::new(std::path::Path::new(&shard_dir));
    let mut loader = ShardLoader::<E, M, S, _, _>::new(fetcher, shard_filename);

    let owned = loader.load(&shard).await?;
    let mut shards = vec![owned];
    for neighbour in strategy.neighbours(&shard) {
        if selection.contains(&neighbour) {
            if let Ok(net) = loader.load(&neighbour).await {
                shards.push(net);
            }
        }
    }

    let network = Arc::new(MultiShardNetwork::<E, M, S>::new(shards));
    let adapter = Arc::new(RPCAdapter::new(network));

    let router = Router::new();
    let router = MatchServiceExt::register(adapter.clone(), router);
    let router = OptimiseServiceExt::register(adapter.clone(), router);
    let router = ScanServiceExt::register(adapter, router);

    tracing::info!(message = "Starting server.", %addr);
    Server::new(router)
        .serve(addr)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(())
}
