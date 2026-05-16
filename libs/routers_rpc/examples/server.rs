extern crate alloc;

use alloc::sync::Arc;
use dotenv::dotenv;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{LOS_ANGELES, LOS_ANGELES_SAVED, fixture};
use routers_rpc::Tracer;
use routers_rpc::services::RPCAdapter;

use connectrpc::{Router, Server};
use schema::connect::routers::api::r#match::v1::MatchServiceExt;
use schema::connect::routers::api::optimise::v1::OptimiseServiceExt;
use schema::connect::routers::api::scan::v1::ScanServiceExt;

type OsmRPCAdapter = RPCAdapter<OsmNetwork, OsmEntryId, OsmEdgeMetadata>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn core::error::Error>> {
    dotenv()?;
    Tracer::init();

    tracing::info!(message = "Loading dataset...");

    let los_angeles = fixture!(LOS_ANGELES);
    let los_angeles_saved = fixture!(LOS_ANGELES_SAVED);
    let network = OsmNetwork::from_pbf_and_save(&los_angeles, &los_angeles_saved)?;

    tracing::info!(message = "Finished loading dataset.");

    let network = Arc::new(network);
    let adapter = Arc::new(OsmRPCAdapter::new(network));

    let router = Router::new();
    let router = MatchServiceExt::register(adapter.clone(), router);
    let router = OptimiseServiceExt::register(adapter.clone(), router);
    let router = ScanServiceExt::register(adapter, router);

    let addr = "[::1]:9001".parse()?;
    tracing::info!(message = "Starting server.", %addr);

    Server::new(router)
        .serve(addr)
        .await
        .map_err(|e| -> Box<dyn core::error::Error> { e.to_string().into() })?;

    Ok(())
}
