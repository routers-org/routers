//! Realtime Data Viewer
//!
//! Subscribes to the matched event subject on NATS and draws each vehicle's
//! latest matched path on a map, grouped (and coloured) by vehicle id.

#![warn(clippy::all, rust_2018_idioms)]

extern crate alloc;

mod app;
mod plugin;
mod store;

use core::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use log::info;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_realtime::bus::NATSStream;
use routers_realtime::event::MatchResult;

use crate::app::RealtimeApp;

type E = OsmEntryId;
type M = OsmEdgeMetadata;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server
    #[arg(short, env, long)]
    nats: String,

    /// The inbound NATS subject to subscribe to, for matched vehicle events
    /// (e.g. `events.matched.*`).
    #[arg(short, env, long)]
    inbound_subject: String,

    /// Maximum path points retained per vehicle, across all matched
    /// windows. Whole windows are evicted oldest-first once exceeded.
    #[arg(long, env, default_value = "1000")]
    trace_capacity: usize,

    /// Vehicles with no events for this many seconds are evicted.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "120s")]
    idle_ttl: Duration,
}

#[tokio::main]
async fn main() -> Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();

    let args = Args::parse();
    info!("realtime viewer started: {:?}", args);

    let client = async_nats::ConnectOptions::new()
        .name("RealtimeViewer")
        .connect(&args.nats)
        .await
        .context("could not connect to NATS")?;

    let subscriber = client
        .subscribe(args.inbound_subject)
        .await
        .context("could not subscribe to NATS subject")?;
    let source = NATSStream::<MatchResult<E, M>>::new(subscriber);

    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("realtime viewer")
            .with_inner_size([1500.0, 950.0])
            .with_min_inner_size([600.0, 400.0]),
        ..Default::default()
    };

    // Share the "routers" storage namespace so the Mapbox API key configured
    // in the main viewer applies here too.
    eframe::run_native(
        "routers",
        native_options,
        Box::new(move |ctx| {
            Ok(Box::new(RealtimeApp::new(
                ctx,
                source,
                args.trace_capacity,
                args.idle_ttl,
            )))
        }),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
