/// Loads and sorts the full dataset, then walks events in
/// chronological order. Publishes directly to the NATS EVENTS JetStream stream.
use clap::Parser;
use log::{debug, info};
use polars::prelude::*;
use std::{path::PathBuf, time::Duration};
use url::Url;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The URL of the input file, to replay
    #[arg(short, long)]
    file: PathBuf,

    /// The URL of the NATS server
    #[arg(short, long)]
    nats: Url,

    /// The replay speed, as a multiplier of the original event rate.
    /// Any negative, or zero-value will default to FLOOD mode, where events are published as fast as possible.
    #[arg(short, long, default_value_t = 1.0)]
    speed: f64,

    /// The number of times to replay the input file.
    /// Defaults to 1, but a higher value can be used for saturation testing.
    #[arg(short, long, default_value_t = 1)]
    loops: usize,

    /// Shard precision level to send the events as
    #[arg(short, long, default_value_t = 5)]
    precision: u8,
}

// 2026-04-01 03:40:02 UTC, or 2026-04-01 03:40:02.123456 UTC
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S %Z";
const TIME_FORMAT_FRACTIONAL: &str = "%Y-%m-%d %H:%M:%S%.f %Z";

// Column names
const VEHICLE_ID_COL: &str = "VehicleID";
const TRIP_ID_COL: &str = "TripID";

const PROVIDER_COL: &str = "Provider";
const EVENT_TIME_COL: &str = "EventTime";

const LATITUDE_COL: &str = "Latitude";
const LONGITUDE_COL: &str = "Longitude";

fn parse_datetime(fmt: &str) -> Expr {
    col(EVENT_TIME_COL).str().to_datetime(
        Some(TimeUnit::Microseconds),
        None,
        StrptimeOptions {
            format: Some(fmt.into()),
            strict: false,
            ..Default::default()
        },
        lit("raise"),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();
    info!("replay starting: {:?}", args);

    let df = LazyCsvReader::new(args.file)
        .with_has_header(true)
        .finish()?
        .sort([EVENT_TIME_COL], SortMultipleOptions::default())
        .select([
            col(TRIP_ID_COL),
            col(VEHICLE_ID_COL),
            col(PROVIDER_COL),
            parse_datetime(TIME_FORMAT).fill_null(parse_datetime(TIME_FORMAT_FRACTIONAL)),
            col(LATITUDE_COL),
            col(LONGITUDE_COL),
        ])
        .collect()
        .map_err(|e| anyhow::anyhow!("dataframe parse: {e}"))?;

    let n = df.height();
    if n == 0 {
        debug!("no events found.");
        return Ok(());
    }

    let times = df.column(EVENT_TIME_COL)?.datetime()?;
    let timespan_s =
        Duration::from_micros(times.max().unwrap() as u64 - times.min().unwrap() as u64)
            .as_secs_f64();
    debug!("loaded {n:>7} events spanning {timespan_s:.1} s");

    let flood = args.speed <= 0.0;
    let speed = if flood { f64::INFINITY } else { args.speed };

    if flood {
        info!("[flood-mode] sending at max rate for loops={0}", args.loops);
    } else {
        info!(
            "[walk-mode] replaying at {speed}x for loops={0} (walltime-per-loop={1:.1} s)",
            args.loops,
            timespan_s / speed,
        );
    }

    for iteration in 0..args.loops {
        debug!("loop {iteration}/{0}", args.loops);

        // Do work.
    }

    Ok(())
}
