use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr};
use clap::Parser;
use futures::StreamExt;
use log::{error, info};
use std::time::Duration;
use tokio::time::{Instant, timeout_at};
use url::Url;

use routers_realtime::{
    bus::NATSStream,
    event::{Payload, RawEvent},
    store::RedisStore,
};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server
    #[arg(short, env, long)]
    nats: Url,

    /// URL of the Redis cluster
    #[arg(short, env, long)]
    redis: Url,

    /// The subject to use for the NATS events stream
    #[arg(long, env, default_value = "events.raw.>")]
    subject: String,

    /// The number of events to keep in the Redis history
    #[arg(long, env, default_value_t = 25)]
    history: usize,

    /// Batch size for Redis publishing
    #[arg(long, env, default_value_t = 1024)]
    batch_size: usize,

    /// Batch timeout for Redis publishing
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "100ms")]
    batch_timeout: Duration,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args = Args::parse();

    info!("historian starting: {:?}", args);

    let nats_url = ServerAddr::from_url(args.nats).context("could not create NATS url")?;

    let client = ConnectOptions::new()
        .name("HistorianService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;
    let subscriber = client
        .subscribe(args.subject)
        .await
        .context("could not subscribe to NATS subject")?;

    let mut nats = NATSStream::<Payload>::new(subscriber);

    let mut kv = RedisStore::<RawEvent>::new(args.redis)
        .await
        .context("could not connect to redis store")?;
    let mut batch: Vec<RawEvent> = Vec::with_capacity(args.batch_size);

    loop {
        batch.clear();
        let deadline = Instant::now() + args.batch_timeout;

        while batch.len() < batch.capacity() {
            match timeout_at(deadline, nats.next()).await {
                Ok(Some(e)) => batch.push(e.as_event()),
                _ => break,
            }
        }

        if batch.is_empty() {
            continue;
        }

        if let Err(e) = kv.write_many(&batch, args.history).await {
            error!("write error: {e}");
        } else {
            info!("archived {} event(s)", batch.len());
        }
    }
}
