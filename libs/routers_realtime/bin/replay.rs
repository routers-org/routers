/// Loads and sorts the full dataset, then walks events in chronological
/// order. Each event is validated and published, broker-acknowledged, through
/// [`Ingress`] — the reference producer for the ingest contract
/// (`routers_realtime::ingress` + `topology`). By default it feeds the live
/// raw journal; with `--isolated <run>` it provisions and feeds a private
/// `replay.<run>.` journal instead, so rematching history never injects
/// historical work into the live deadline path.
///
/// # Throughput
///
/// A single chronological walker keeps the pacing (`--speed`) and the progress
/// bar authoritative, but publishing itself fans out across `--lanes` tasks so
/// flood mode is not one broker round-trip per row. Each row is routed to a
/// lane by `partition_of(vehicle) % lanes`, so a vehicle's observations always
/// land on one lane and stay strictly ordered (revisions are stream sequences,
/// so per-vehicle send order is load-bearing); distinct lanes overlap their
/// round-trips. The walker hands rows to lanes over bounded channels, so a slow
/// broker back-pressures the walk instead of buffering without bound.
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
    partition, topology,
};
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;
use url::Url;

/// Bounded depth of each lane's hand-off channel. Deep enough that a lane never
/// starves between rows, shallow enough that a stalled broker back-pressures the
/// walker (and so the pacing/progress bar) instead of buffering the whole file.
const LANE_CHANNEL_CAPACITY: usize = 1024;

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

    /// How many publish lanes to fan sends across. Each vehicle is pinned to one
    /// lane by `partition_of(vehicle) % lanes`, so its observations stay in send
    /// order while distinct lanes overlap their broker round-trips — the flood
    /// mode's throughput knob. `1` reproduces the fully-serial behaviour; `0` is
    /// treated as `1`.
    #[arg(long, env, default_value_t = 64)]
    lanes: usize,

    /// How many raw streams the partition space divides across, fleet-wide.
    /// Fixed config: revisions are stream sequences, so remapping partitions
    /// to different streams is a migration, not a tuning knob.
    #[arg(long, env, default_value_t = 4)]
    streams: u64,

    /// Replay into an isolated processing run instead of the live journal.
    /// Subjects and streams are prefixed `replay.<run>.`, so the historical
    /// events never enter the live deadline path and age out on their own.
    /// `<run>` must be NATS-safe (`[A-Za-z0-9_-]+`).
    #[arg(long, env = "REPLAY_ISOLATED")]
    isolated: Option<String>,

    /// Reject observations older than this before publishing (a humantime
    /// duration, e.g. `7days`, `36h`). A historical backfill should pass a
    /// large value — e.g. `--max-age 3650days` — so old rows are admitted
    /// rather than dropped. Defaults to `IngressLimits::default()` (7 days).
    #[arg(long, env, value_parser = humantime::parse_duration)]
    max_age: Option<Duration>,

    /// Reject observations whose timestamp is more than this far in the future
    /// (a humantime duration, e.g. `5min`). Guards against clock skew, not
    /// history. Defaults to `IngressLimits::default()` (5 minutes).
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

    // Age/skew limits: whatever the caller passed, otherwise the shared
    // ingress defaults.
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

    // At least one lane; `--lanes 0` is a no-op knob, not an empty run.
    let lanes = args.lanes.max(1);

    // Spin up the publish lanes. Each owns a clone of `Ingress` (one shared
    // connection) and drains a bounded channel; a vehicle is pinned to one lane
    // so its sends stay ordered, while distinct lanes overlap their round-trips.
    // A broker publish failure surfaces as a lane returning `Err`, propagated
    // through the `JoinSet`; validation faults are tallied per lane instead.
    let mut senders: Vec<mpsc::Sender<Payload>> = Vec::with_capacity(lanes);
    let mut set: JoinSet<anyhow::Result<LaneReport>> = JoinSet::new();
    for _ in 0..lanes {
        let (tx, rx) = mpsc::channel::<Payload>(LANE_CHANNEL_CAPACITY);
        senders.push(tx);
        set.spawn(run_lane(ingress.clone(), rx));
    }
    // The walker no longer publishes, so its own clone is redundant; keep the
    // lanes' clones the only live handles.
    drop(ingress);

    // A lane that exits early (a broker failure) is captured here so the walk
    // can stop feeding and the run can fail with its error.
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

            // Route by vehicle so a vehicle's observations always take one lane
            // and cannot transpose; the lane awaits each send in order, exactly
            // as the serial walk used to.
            let lane = lane_of(payload.vehicle_id, lanes);
            if senders[lane].send(payload).await.is_err() {
                // The receiver is gone: this lane exited early on a broker
                // failure. Stop feeding; the error is collected below.
                break 'walk;
            }

            // Cheap early-abort: a healthy lane never finishes while its channel
            // is open, so a ready join means a lane failed. Stop promptly rather
            // than pushing the rest of the file at a dying broker.
            if let Some(joined) = set.try_join_next() {
                early = Some(joined);
                break 'walk;
            }
        }
        pg.finish();
    }

    // Close the channels so every healthy lane drains its buffer and returns its
    // tally; then fold the lanes together, surfacing the first broker failure.
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

/// The lane a vehicle's observations are routed to: `partition_of(vehicle) %
/// lanes`. Deterministic in the vehicle, so every observation of one vehicle
/// hashes to the same lane and their sends never race. `lanes` must be at least
/// one (the caller clamps it).
fn lane_of(vehicle: VehicleId, lanes: usize) -> usize {
    (partition::partition_of(vehicle) % lanes as u64) as usize
}

/// One publish lane: drain the channel, publishing each observation in receive
/// order through this lane's [`Ingress`] clone. Validation faults are tallied
/// (one bad row is skipped, never fatal); a broker publish failure returns
/// `Err`, which the `JoinSet` propagates so the whole run aborts.
async fn run_lane(ingress: Ingress, mut rx: mpsc::Receiver<Payload>) -> anyhow::Result<LaneReport> {
    let mut report = LaneReport::default();
    while let Some(payload) = rx.recv().await {
        // Await each publish before the next: JetStream assigns revisions in
        // per-connection send order, so awaiting in receive order is what keeps
        // this lane's (single) vehicles ordered downstream.
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

/// The running tally one lane reports back: how many observations it published,
/// how many the broker recognised as duplicates of an earlier send, and the
/// per-variant count of rows validation rejected. All bounded labels — no
/// vehicle id or coordinate ever enters it.
#[derive(Debug, Default)]
struct LaneReport {
    /// Observations the broker acknowledged (duplicates included).
    published: u64,
    /// Of `published`, those the broker collapsed onto an earlier identical send.
    duplicates: u64,
    /// Rejected rows, keyed by the fixed [`IngressError::kind`] label.
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

/// Log the aggregate outcome of a run: how much was published, and the
/// per-variant tally of rows validation rejected (or a note that none were).
/// Bounded to fixed labels, so it never prints a vehicle id or coordinate.
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

    /// Every observation of one vehicle must take the same lane, whatever the
    /// lane count — that pin is what keeps its sends strictly ordered.
    #[test]
    fn lane_is_stable_per_vehicle() {
        for lanes in [1usize, 2, 7, 64, 1000] {
            for raw in [0u64, 1, 7, 42, 1_000, u64::MAX, 0x9E37_79B9_7F4A_7C15] {
                let vehicle = VehicleId(raw);
                let first = lane_of(vehicle, lanes);
                // Recomputing (as each of a vehicle's rows does) is identical.
                assert_eq!(first, lane_of(vehicle, lanes), "raw={raw} lanes={lanes}");
                assert!(first < lanes, "lane {first} out of range for {lanes} lanes");
            }
        }
    }

    /// A single lane collapses every vehicle onto lane 0 — the fully-serial
    /// fallback that reproduces the pre-lane behaviour.
    #[test]
    fn one_lane_pins_everything_to_zero() {
        for raw in [1u64, 2, 3, 99, u64::MAX] {
            assert_eq!(lane_of(VehicleId(raw), 1), 0);
        }
    }

    /// Across many vehicles the assignment spreads: with 64 lanes and a few
    /// thousand distinct ids, sends are not funnelled through one task.
    #[test]
    fn lanes_spread_across_many_vehicles() {
        let lanes = 64;
        let used: HashSet<usize> = (1..=4096u64)
            .map(|raw| lane_of(VehicleId(raw), lanes))
            .collect();
        // The partitioner mixes its input, so thousands of ids should touch the
        // large majority of lanes; assert a generous floor to stay non-flaky.
        assert!(
            used.len() >= lanes / 2,
            "only {} of {lanes} lanes used",
            used.len(),
        );
    }

    /// The lane knob defaults to 64 when the caller does not pass `--lanes`.
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

    /// An explicit `--lanes` overrides the default.
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

    /// Aggregation sums published/duplicate counts and unions rejection tallies.
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

    /// A lane's broker failure is the first recorded, and does not discard the
    /// tallies of lanes that succeeded.
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
