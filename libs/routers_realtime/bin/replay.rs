/// Loads and sorts the full dataset, then walks events in chronological
/// order, validating and publishing each through [`Ingress`]. By default it
/// feeds the live raw journal; `--isolated <run>` feeds a private
/// `replay.<run>.` journal instead. Publishing fans out across `--lanes` tasks,
/// each vehicle pinned to one lane so its send order (load-bearing: revisions
/// are stream sequences) is preserved, while lanes overlap their round-trips.
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
    ingress::Ingress,
    partition,
    secret::SecretUrl,
    topology,
};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

/// Bounded depth of each lane's hand-off channel, so a stalled broker back-pressures the walker rather than buffering the whole file.
const LANE_CHANNEL_CAPACITY: usize = 1024;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The URL of the input file, to replay
    #[arg(short, env, long)]
    file: PathBuf,

    /// The URL of the NATS server
    #[arg(short, env, long)]
    nats: SecretUrl,

    /// The replay speed, as a multiplier of the original event rate.
    /// Any negative, or zero-value will default to FLOOD mode, where events are published as fast as possible.
    #[arg(short, env, long, default_value_t = 1.0)]
    speed: f64,

    /// The number of times to replay the input file.
    /// Defaults to 1, but a higher value can be used for saturation testing.
    #[arg(short, env, long, default_value_t = 1)]
    loops: usize,

    /// How many publish lanes to fan sends across; each vehicle is pinned to one lane. `0` is treated as `1`.
    #[arg(long, env, default_value_t = 64)]
    lanes: usize,

    /// How many raw streams the partition space divides across, fleet-wide.
    /// Fixed config: revisions are stream sequences, so remapping partitions
    /// to different streams is a migration, not a tuning knob.
    #[arg(long, env, default_value_t = 4)]
    streams: u64,

    /// Replay into an isolated run (subjects/streams prefixed `replay.<run>.`); `<run>` must be NATS-safe (`[A-Za-z0-9_-]+`).
    #[arg(long, env = "REPLAY_ISOLATED")]
    isolated: Option<String>,
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
        .connect(ServerAddr::from_url(args.nats.connection_url())?)
        .await?;

    let context = jetstream::new(client);

    let ingress = match &args.isolated {
        Some(run) => {
            info!("isolated replay run: {run:?}");
            Ingress::isolated(context, run)
                .with_context(|| format!("invalid --isolated run token {run:?}"))?
        }
        None => Ingress::live(context),
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

    // At least one lane; `--lanes 0` is a no-op knob, not an empty run.
    let lanes = args.lanes.max(1);

    // Each lane owns an `Ingress` clone and drains a bounded channel; a broker failure surfaces as the lane returning `Err`.
    let mut senders: Vec<mpsc::Sender<Payload>> = Vec::with_capacity(lanes);
    let mut set: JoinSet<anyhow::Result<LaneReport>> = JoinSet::new();
    for _ in 0..lanes {
        let (tx, rx) = mpsc::channel::<Payload>(LANE_CHANNEL_CAPACITY);
        senders.push(tx);
        set.spawn(run_lane(ingress.clone(), rx));
    }
    // The walker no longer publishes; drop its redundant clone.
    drop(ingress);

    // A lane that exits early (broker failure) is captured here.
    let mut early: Option<Result<anyhow::Result<LaneReport>, JoinError>> = None;

    'walk: for iteration in 0..args.loops {
        pg.reset();

        debug!("loop {iteration}/{0}", args.loops);
        let rows = rows_of(&df).context("could not deserialize rows from dataframe")?;

        let start = Instant::now();
        for (time, payload) in rows {
            pg.inc(1);

            let offset = Duration::from_micros(time - min).div_f64(speed);
            tokio::time::sleep_until(start + offset).await;

            // Route by vehicle so its observations stay on one lane and cannot transpose.
            let lane = lane_of(payload.vehicle_id, lanes);
            if senders[lane].send(payload).await.is_err() {
                // Receiver gone: this lane exited early; stop feeding.
                break 'walk;
            }

            // A healthy lane never finishes while its channel is open, so a ready join means a lane failed.
            if let Some(joined) = set.try_join_next() {
                early = Some(joined);
                break 'walk;
            }
        }
        pg.finish();
    }

    // Close the channels so every lane drains and returns its tally.
    drop(senders);

    let mut totals = LaneReport::default();
    let mut failure: Option<anyhow::Error> = None;
    if let Some(joined) = early.take() {
        absorb(&mut totals, &mut failure, joined);
    }
    while let Some(joined) = set.join_next().await {
        absorb(&mut totals, &mut failure, joined);
    }

    multi.remove(&pg);

    if let Some(error) = failure {
        return Err(error);
    }

    report_totals(&totals);

    Ok(())
}

/// The lane a vehicle's observations are routed to: `partition_of(vehicle) % lanes`.
/// Deterministic in the vehicle, so one vehicle's sends never race across lanes.
fn lane_of(vehicle: VehicleId, lanes: usize) -> usize {
    (partition::partition_of(vehicle) % lanes as u64) as usize
}

/// One publish lane: drain the channel, publishing each observation in receive
/// order. A validation fault is tallied and skipped; a broker failure returns `Err`.
async fn run_lane(ingress: Ingress, mut rx: mpsc::Receiver<Payload>) -> anyhow::Result<LaneReport> {
    let mut report = LaneReport::default();
    while let Some(payload) = rx.recv().await {
        // Await each publish before the next: revisions are assigned in send order, so receive order must be preserved.
        match ingress.publish(&payload, bus::wallclock()).await {
            Ok(ack) => {
                report.published += 1;
                if ack.duplicate {
                    report.duplicates += 1;
                }
            }
            Err(error) if error.is_data_fault() => {
                *report.rejected.entry(error.kind()).or_default() += 1;
            }
            Err(error) => {
                return Err(anyhow::Error::new(error).context("could not publish event"));
            }
        }
    }
    Ok(report)
}

/// The running tally one lane reports back. All bounded labels — no vehicle id or coordinate ever enters it.
#[derive(Debug, Default)]
struct LaneReport {
    /// Observations the broker acknowledged (duplicates included).
    published: u64,
    /// Of `published`, those the broker collapsed onto an earlier identical send.
    duplicates: u64,
    /// Rejected rows, keyed by the fixed `IngressError::kind` label.
    rejected: BTreeMap<&'static str, u64>,
}

impl LaneReport {
    /// Fold another lane's tally into this one.
    fn merge(&mut self, other: LaneReport) {
        self.published += other.published;
        self.duplicates += other.duplicates;
        for (kind, count) in other.rejected {
            *self.rejected.entry(kind).or_default() += count;
        }
    }
}

/// Fold one joined lane result into the totals, recording the first broker
/// failure (or a lane panic) without discarding the other lanes' tallies.
fn absorb(
    totals: &mut LaneReport,
    failure: &mut Option<anyhow::Error>,
    joined: Result<anyhow::Result<LaneReport>, JoinError>,
) {
    match joined {
        Ok(Ok(report)) => totals.merge(report),
        Ok(Err(error)) if failure.is_none() => *failure = Some(error),
        Ok(Err(_)) => {}
        Err(join) if failure.is_none() => {
            *failure = Some(anyhow::Error::new(join).context("a publish lane panicked"));
        }
        Err(_) => {}
    }
}

/// Log the aggregate outcome of a run; bounded to fixed labels, never a vehicle id or coordinate.
fn report_totals(totals: &LaneReport) {
    if totals.duplicates == 0 {
        info!(
            "replay complete: {} observation(s) published",
            totals.published
        );
    } else {
        info!(
            "replay complete: {} observation(s) published ({} broker duplicate(s))",
            totals.published, totals.duplicates,
        );
    }

    if totals.rejected.is_empty() {
        info!("no observations rejected");
        return;
    }

    let total: u64 = totals.rejected.values().sum();
    warn!("{total} observation(s) rejected by validation");
    for (kind, count) in &totals.rejected {
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

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn lane_is_stable_per_vehicle() {
        for lanes in [1usize, 2, 7, 64, 1000] {
            for raw in [0u64, 1, 7, 42, 1_000, u64::MAX, 0x9E37_79B9_7F4A_7C15] {
                let vehicle = VehicleId(raw);
                let first = lane_of(vehicle, lanes);
                assert_eq!(first, lane_of(vehicle, lanes), "raw={raw} lanes={lanes}");
                assert!(first < lanes, "lane {first} out of range for {lanes} lanes");
            }
        }
    }

    #[test]
    fn one_lane_pins_everything_to_zero() {
        for raw in [1u64, 2, 3, 99, u64::MAX] {
            assert_eq!(lane_of(VehicleId(raw), 1), 0);
        }
    }

    #[test]
    fn lanes_spread_across_many_vehicles() {
        let lanes = 64;
        let used: HashSet<usize> = (1..=4096u64)
            .map(|raw| lane_of(VehicleId(raw), lanes))
            .collect();
        // Assert a generous floor (mixing spreads ids) to stay non-flaky.
        assert!(
            used.len() >= lanes / 2,
            "only {} of {lanes} lanes used",
            used.len(),
        );
    }

    #[test]
    fn lanes_default_is_sixty_four() {
        let args = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
        ])
        .expect("minimal args should parse");
        assert_eq!(args.lanes, 64);
    }

    #[test]
    fn lanes_arg_is_honoured() {
        let args = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
            "--lanes",
            "8",
        ])
        .expect("args with --lanes should parse");
        assert_eq!(args.lanes, 8);
    }

    #[test]
    fn lane_report_merge_folds_tallies() {
        let mut totals = LaneReport::default();
        let mut a = LaneReport {
            published: 10,
            duplicates: 2,
            ..Default::default()
        };
        *a.rejected.entry("too_old").or_default() += 3;
        let mut b = LaneReport {
            published: 5,
            duplicates: 1,
            ..Default::default()
        };
        *b.rejected.entry("too_old").or_default() += 4;
        *b.rejected.entry("zero_vehicle").or_default() += 1;

        totals.merge(a);
        totals.merge(b);

        assert_eq!(totals.published, 15);
        assert_eq!(totals.duplicates, 3);
        assert_eq!(totals.rejected.get("too_old"), Some(&7));
        assert_eq!(totals.rejected.get("zero_vehicle"), Some(&1));
    }

    #[test]
    fn absorb_records_first_failure_and_keeps_tallies() {
        let mut totals = LaneReport::default();
        let mut failure = None;

        absorb(
            &mut totals,
            &mut failure,
            Ok(Ok(LaneReport {
                published: 4,
                ..Default::default()
            })),
        );
        absorb(
            &mut totals,
            &mut failure,
            Ok(Err(anyhow::anyhow!("broker down"))),
        );
        absorb(
            &mut totals,
            &mut failure,
            Ok(Err(anyhow::anyhow!("later, ignored"))),
        );

        assert_eq!(totals.published, 4);
        assert_eq!(
            failure
                .expect("a broker failure should be recorded")
                .to_string(),
            "broker down",
        );
    }
}
