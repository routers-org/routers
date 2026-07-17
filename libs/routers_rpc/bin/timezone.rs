extern crate alloc;

use alloc::sync::Arc;
use core::net::SocketAddr;

use clap::Parser;
use connectrpc::{Router, Server};
use routers_rpc::services::timezone::TimezoneAdapter;
use routers_tz::S2CellStorage;
use schema::connect::routers::api::timezone::v1::TimezoneServiceExt;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// Address for the server to listen on
    #[arg(short, long, env, default_value = "[::]:9001")]
    addr: SocketAddr,
}

use log::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn core::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .compact()
        .init();

    let args = Args::parse();
    info!("realtime starting: {:?}", args);

    let storage = Arc::new(S2CellStorage::default());
    let adapter = Arc::new(TimezoneAdapter::new(storage));

    info!("loaded timezone storage");
    let router = TimezoneServiceExt::register(adapter, Router::new());

    info!("starting server, addr={}", args.addr);
    Server::new(router)
        .serve(args.addr)
        .await
        .map_err(|e| -> Box<dyn core::error::Error> { e.to_string().into() })?;

    Ok(())
}
