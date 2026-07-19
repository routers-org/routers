//! Realtime Data Viewer

#![warn(clippy::all, rust_2018_idioms)]

mod app;

use anyhow::Result;
use clap::Parser;
use log::info;

use crate::app::RealtimeApp;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    // The inbound NATS subject to subscribe to, for matching events.
    #[arg(short, env, long)]
    inbound_subject: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let args = Args::parse();
    info!("realtime viewer started: {:?}", args);

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
        Box::new(move |ctx| Ok(Box::new(RealtimeApp::new(ctx)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
