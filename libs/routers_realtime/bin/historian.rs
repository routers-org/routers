use clap::Parser;
use futures::StreamExt;
use log::info;
use routers::shard::{Geohash, GeohashStrategy};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};
use url::Url;

use async_nats::{ServerAddr, connect, jetstream};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server
    #[arg(short, long)]
    nats: Url,

    /// URL of the Redis cluster
    #[arg(short, long)]
    redis: Url,

    /// Shard precision level
    #[arg(short, long, default_value_t = 5)]
    shard_precision: u8,

    /// Batch size for Redis publishing
    #[arg(short, long, default_value_t = 256)]
    batch_size: usize,

    /// Batch timeout for Redis publishing
    #[arg(short, long, value_parser = humantime::parse_duration, default_value = "100ms")]
    batch_timeout: Duration,
}

struct Position {/* TODO */}
type Event = (String, Geohash, Position);

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();

    info!("historian starting: {:#?}", args);

    let _strategy = GeohashStrategy::with_precision(args.shard_precision);

    let nats_url =
        ServerAddr::from_url(args.nats).map_err(|e| anyhow::anyhow!("NATS URL parse: {e}"))?;

    let nc = connect(nats_url)
        .await
        .map_err(|e| anyhow::anyhow!("NATS connect: {e}"))?;

    let _jetstream = jetstream::new(nc);

    let mut batch: Vec<Event> = Vec::with_capacity(args.batch_size);

    loop {
        batch.clear();
        let deadline = Instant::now() + args.batch_timeout;

        // TODO: Create event stream
        let mut events = futures::stream::iter(Vec::<Event>::new());

        while let Ok(Some(e)) = timeout_at(deadline, events.next()).await {
            batch.push(e);

            if batch.len() >= args.batch_size {
                break;
            }
        }

        // if let Err(e) = valkey.write_many(&batch).await {
        //     eprintln!("historian: Valkey write error: {e}");
        // }
    }
}
