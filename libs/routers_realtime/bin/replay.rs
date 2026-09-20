/// Loads and sorts the dataset, then publishes each event through [`Ingress`]
/// in chronological order. By default it feeds the live journal;
/// `--isolated <run>` feeds a private replay journal.
extern crate alloc;

use alloc::collections::BTreeMap;
use core::fmt::Write;
use core::time::Duration;
use std::path::PathBuf;

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr, jetstream};
use clap::Parser;
use fnv_rs::{Fnv64, FnvHasher};
use geo::Point;
use indicatif::{ProgressBar, ProgressState, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
use itertools::izip;
use log::{debug, info, warn};
use polars::prelude::*;
use routers_realtime::{
    bus,
    event::{Payload, VehicleId},
    ingress::{Ingress, IngressLimits},
    topology,
};
use tokio::time::Instant;
use url::Url;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The URL of the input file, to replay
    #[arg(short, env, long)]
    file: PathBuf,

    /// The URL of the NATS server
    #[arg(short, env, long)]
    nats: Url,

    /// The replay speed, as a multiplier of the original event rate.
    /// Any negative, or zero-value will default to FLOOD mode, where events are published as fast as possible.
    #[arg(short, env, long, default_value_t = 1.0)]
    speed: f64,

    /// The number of times to replay the input file.
    /// Defaults to 1, but a higher value can be used for saturation testing.
    #[arg(short, env, long, default_value_t = 1)]
    loops: usize,

    /// How many raw streams the partition space divides across, fleet-wide.
    /// Fixed config: revisions are stream sequences, so remapping partitions
    /// to different streams is a migration, not a tuning knob.
    #[arg(long, env, default_value_t = 4)]
    streams: u64,

    /// Replay into an isolated run (subjects/streams prefixed `replay.<run>.`); `<run>` must be NATS-safe (`[A-Za-z0-9_-]+`).
    #[arg(long, env = "REPLAY_ISOLATED")]
    isolated: Option<String>,

    /// Reject observations older than this before publishing (humantime, e.g. `7days`). Defaults to `IngressLimits::default()` (7 days).
    #[arg(long, env, value_parser = humantime::parse_duration)]
    max_age: Option<Duration>,

    /// Reject observations more than this far in the future (humantime, e.g. `5min`); guards clock skew. Defaults to 5 minutes.
    #[arg(long, env, value_parser = humantime::parse_duration)]
    max_ahead: Option<Duration>,
}

// 2026-04-01 03:40:02 UTC, or 2026-04-01 03:40:02.123456 UTC
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M:%S %Z";
const TIME_FORMAT_FRACTIONAL: &str = "%Y-%m-%d %H:%M:%S%.f %Z";

// Column names
const VEHICLE_ID_COL: &str = "VehicleID";

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
    let logger = env_logger::Builder::from_default_env().build();
    let level = logger.filter();

    let multi = indicatif::MultiProgress::new();
    LogWrapper::new(multi.clone(), logger).try_init().unwrap();
    log::set_max_level(level);

    let args = Args::parse();
    info!("replay starting: {:?}", args);

    let client = ConnectOptions::new()
        .name("ReplayService")
        .connect(ServerAddr::from_url(args.nats)?)
        .await?;

    let context = jetstream::new(client);

    let defaults = IngressLimits::default();
    let limits = IngressLimits {
        max_age: args.max_age.unwrap_or(defaults.max_age),
        max_ahead: args.max_ahead.unwrap_or(defaults.max_ahead),
    };

    let ingress = match &args.isolated {
        Some(run) => {
            info!("isolated replay run: {run:?}");
            Ingress::isolated(context, run, limits)
                .with_context(|| format!("invalid --isolated run token {run:?}"))?
        }
        None => Ingress::live(context, limits),
    };

    let raw_cfg = topology::RawConfig {
        streams: args.streams,
        ..Default::default()
    };
    ingress.ensure_streams(args.streams, &raw_cfg).await?;

    let df = LazyCsvReader::new(args.file)
        .with_has_header(true)
        .finish()?
        .sort([EVENT_TIME_COL], SortMultipleOptions::default())
        .select([
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
    let (min, max) = (times.min().unwrap() as u64, times.max().unwrap() as u64);
    let timespan_s = Duration::from_micros(max - min).as_secs_f64();

    debug!("loaded {n:>7} events spanning {timespan_s:.1} s");

    let flood = args.speed <= 0.0;
    let speed = if flood { f64::INFINITY } else { args.speed };
    let realtime_s = if flood { 0.0 } else { timespan_s / speed };

    let pb = ProgressBar::new(df.height() as u64);
    pb.set_style(
        ProgressStyle::with_template(
            "[{elapsed_precise}] {bar:40.cyan/blue} {pos:>7}/{len:7} {msg} ({speed} evt/s sent)",
        )
        .unwrap()
        .progress_chars("##-")
        .with_key("speed", |state: &ProgressState, w: &mut dyn Write| {
            write!(w, "{:>6.1}", state.per_sec()).unwrap();
        }),
    );
    let pg = multi.add(pb);

    if flood {
        pg.set_message("[flood-mode] speed=∞x".to_string());
    } else {
        pg.set_message(format!(
            "[walk-mode] speed={speed}x (walltime={timespan_s:.1} s, realtime={realtime_s:.1} s)"
        ));
    }

    // Tally rejected observations by variant so one bad row is skipped and
    // counted, never fatal. A publish (broker) failure is different — that is
    // infrastructure, and it stops the run.
    let mut rejected: BTreeMap<&'static str, u64> = BTreeMap::new();

    for iteration in 0..args.loops {
        pg.reset();

        debug!("loop {iteration}/{0}", args.loops);
        let rows = rows_of(&df).context("could not deserialize rows from dataframe")?;

        let start = Instant::now();
        for (time, payload) in rows {
            pg.inc(1);

            let offset = Duration::from_micros(time - min).div_f64(speed);
            tokio::time::sleep_until(start + offset).await;

            // Await each publish before the next: JetStream assigns revisions
            // in per-connection send order, so awaiting in loop order is what
            // keeps a vehicle's observations in order downstream.
            match ingress.publish(&payload, bus::wallclock()).await {
                Ok(_) => {}
                Err(error) if error.is_data_fault() => {
                    *rejected.entry(error.kind()).or_default() += 1;
                }
                Err(error) => {
                    return Err(anyhow::Error::new(error).context("could not publish event"));
                }
            }
        }
        pg.finish();
    }

    multi.remove(&pg);

    report_rejections(&rejected);

    Ok(())
}

/// Log the per-variant tally of observations that failed validation, or note a
/// clean run. Bounded to the fixed [`IngressError::kind`] labels, so it never
/// prints a vehicle id or coordinate.
fn report_rejections(rejected: &BTreeMap<&'static str, u64>) {
    if rejected.is_empty() {
        info!("replay complete: no observations rejected");
        return;
    }

    let total: u64 = rejected.values().sum();
    warn!("replay complete: {total} observation(s) rejected by validation");
    for (kind, count) in rejected {
        warn!("  {kind}: {count}");
    }
}

fn rows_of(df: &DataFrame) -> PolarsResult<impl Iterator<Item = (u64, Payload)> + '_> {
    let vehicle = df.column(VEHICLE_ID_COL)?.str()?;
    let provider = df.column(PROVIDER_COL)?.str()?;
    let etime = df.column(EVENT_TIME_COL)?.datetime()?;
    let lat = df.column(LATITUDE_COL)?.f64()?;
    let lon = df.column(LONGITUDE_COL)?.f64()?;

    Ok(izip!(
        vehicle.into_iter(),
        provider.into_iter(),
        etime.into_iter(),
        lat.into_iter(),
        lon.into_iter()
    )
    .filter_map(|(vehicle, _, etime, lat, lon)| {
        // The id contract (schema: realtime/v1/event.proto): the FNV-1a
        // 64-bit hash of the upstream string, as an integer. `as_bytes`
        // yields it big-endian.
        let vehicle_id: [u8; 8] = Fnv64::hash(vehicle?).as_bytes().try_into().ok()?;

        let payload = Payload {
            vehicle_id: VehicleId(u64::from_be_bytes(vehicle_id)),
            // The column is parsed as microseconds since the Unix epoch.
            timestamp: chrono::DateTime::from_timestamp_micros(etime.unwrap_or_default())
                .unwrap_or_default(),
            point: Point::new(lon.unwrap(), lat.unwrap()),
        };

        Some((etime.unwrap() as u64, payload))
    }))
}
