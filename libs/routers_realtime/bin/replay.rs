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
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
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
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinSet};
use tokio::time::Instant;

/// Bounded depth of each lane's hand-off channel, so a stalled broker back-pressures the walker rather than buffering the whole file.
const LANE_CHANNEL_CAPACITY: usize = 1024;
const TIMING_BUCKET_US: [u64; 21] = [
    10, 25, 50, 100, 250, 500, 1_000, 2_000, 5_000, 10_000, 20_000, 50_000, 100_000, 250_000,
    500_000, 1_000_000, 2_000_000, 5_000_000, 10_000_000, 30_000_000, 60_000_000,
];

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

    /// Publish at this fixed fleet-wide rate (events/second), independent of
    /// timestamps. This is useful for repeatable capacity tests and cannot be
    /// combined with an explicit `--speed`.
    #[arg(long, env, conflicts_with = "speed")]
    rate: Option<f64>,

    /// The number of times to replay the input file.
    /// Defaults to 1, but a higher value can be used for saturation testing.
    #[arg(short, env, long, default_value_t = 1)]
    loops: usize,

    /// Publish at most this many sorted input rows per loop, for short diagnostic runs.
    #[arg(long, env)]
    max_events: Option<usize>,

    /// Skip this many sorted rows before the diagnostic window begins.
    #[arg(long, env, default_value_t = 0)]
    skip_events: usize,

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

    /// Also write the final machine-readable run summary to this JSON file.
    /// The same JSON object is always emitted on stdout.
    #[arg(long, env)]
    summary_json: Option<PathBuf>,
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

    let multi = indicatif::MultiProgress::with_draw_target(ProgressDrawTarget::stderr_with_hz(10));
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

    let load_started = Instant::now();
    let df = LazyCsvReader::new(args.file.clone())
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
    let input_load_seconds = load_started.elapsed().as_secs_f64();

    let n = df.height();
    if n == 0 {
        debug!("no events found.");
        return Ok(());
    }

    let times = df.column(EVENT_TIME_COL)?.datetime()?;
    let (min, max) = (times.min().unwrap() as u64, times.max().unwrap() as u64);
    let timespan_s = Duration::from_micros(max - min).as_secs_f64();

    debug!("loaded {n:>7} events spanning {timespan_s:.1} s");

    let rate = args
        .rate
        .map(|rate| {
            anyhow::ensure!(
                rate.is_finite() && rate > 0.0,
                "--rate must be a positive finite number"
            );
            Ok::<_, anyhow::Error>(rate)
        })
        .transpose()?;
    let flood = rate.is_none() && args.speed <= 0.0;
    let speed = if flood { f64::INFINITY } else { args.speed };
    let realtime_s = if flood { 0.0 } else { timespan_s / speed };

    let available_rows = n.saturating_sub(args.skip_events);
    let selected_rows = args
        .max_events
        .map_or(available_rows, |max| available_rows.min(max));
    let pb = ProgressBar::new(selected_rows as u64);
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

    if let Some(rate) = rate {
        pg.set_message(format!("[fixed-rate] rate={rate:.1} event/s"));
    } else if flood {
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
    let replay_started = Instant::now();
    let mut pacing_sleep = Timing::default();
    let mut schedule_lag = Timing::default();
    let mut lane_send = Timing::default();
    'walk: for iteration in 0..args.loops {
        pg.reset();

        debug!("loop {iteration}/{0}", args.loops);
        let rows = rows_of(&df).context("could not deserialize rows from dataframe")?;

        let start = Instant::now();
        let mut first_time = None;
        for (offset_index, (time, payload)) in
            rows.skip(args.skip_events).take(selected_rows).enumerate()
        {
            pg.inc(1);

            let offset = rate.map_or_else(
                || Duration::from_micros(time - *first_time.get_or_insert(time)).div_f64(speed),
                |rate| Duration::from_secs_f64(offset_index as f64 / rate),
            );
            if !flood {
                let deadline = start + offset;
                let sleep_started = Instant::now();
                tokio::time::sleep_until(deadline).await;
                pacing_sleep.record(sleep_started.elapsed());
                schedule_lag.record(Instant::now().saturating_duration_since(deadline));
            }

            // Route by vehicle so its observations stay on one lane and cannot transpose.
            let lane = lane_of(payload.vehicle_id, lanes);
            let send_started = Instant::now();
            let sent = senders[lane].send(payload).await;
            lane_send.record(send_started.elapsed());
            if sent.is_err() {
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

    let summary = ReplaySummary::new(
        &args,
        n as u64,
        selected_rows as u64,
        rate,
        replay_started.elapsed().as_secs_f64(),
        ReplayTiming {
            input_load_seconds,
            pacing_sleep_us: pacing_sleep.summary(),
            schedule_lag_us: schedule_lag.summary(),
            lane_send_us: lane_send.summary(),
        },
        totals,
    );
    report_totals(&summary);
    let machine = serde_json::to_string(&summary).context("could not encode replay summary")?;
    println!("{machine}");
    if let Some(path) = &args.summary_json {
        tokio::fs::write(path, format!("{machine}\n"))
            .await
            .with_context(|| format!("could not write replay summary to {}", path.display()))?;
    }

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
        report.attempted += 1;
        // Await each publish before the next: revisions are assigned in send order, so receive order must be preserved.
        let publish_started = Instant::now();
        let result = ingress.publish(&payload, bus::wallclock()).await;
        report.publish_ack.record(publish_started.elapsed());
        match result {
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
    /// Observations actually received by a lane for a publish attempt.
    attempted: u64,
    /// Observations the broker acknowledged (duplicates included).
    published: u64,
    /// Of `published`, those the broker collapsed onto an earlier identical send.
    duplicates: u64,
    /// Rejected rows, keyed by the fixed `IngressError::kind` label.
    rejected: BTreeMap<&'static str, u64>,
    publish_ack: Timing,
}

impl LaneReport {
    /// Fold another lane's tally into this one.
    fn merge(&mut self, other: LaneReport) {
        self.attempted += other.attempted;
        self.published += other.published;
        self.duplicates += other.duplicates;
        for (kind, count) in other.rejected {
            *self.rejected.entry(kind).or_default() += count;
        }
        self.publish_ack.merge(other.publish_ack);
    }
}

#[derive(Debug, Default)]
struct Timing {
    buckets: [u64; TIMING_BUCKET_US.len() + 1],
    count: u64,
    total_us: u128,
    max_us: u64,
}

impl Timing {
    fn record(&mut self, duration: Duration) {
        let micros = u64::try_from(duration.as_micros()).unwrap_or(u64::MAX);
        let bucket = TIMING_BUCKET_US.partition_point(|&upper| upper < micros);
        self.buckets[bucket] += 1;
        self.count += 1;
        self.total_us += u128::from(micros);
        self.max_us = self.max_us.max(micros);
    }

    fn merge(&mut self, other: Self) {
        for (bucket, count) in self.buckets.iter_mut().zip(other.buckets) {
            *bucket += count;
        }
        self.count += other.count;
        self.total_us += other.total_us;
        self.max_us = self.max_us.max(other.max_us);
    }

    fn summary(&self) -> TimingSummary {
        if self.count == 0 {
            return TimingSummary {
                count: 0,
                mean_us: None,
                p50_upper_us: 0,
                p95_upper_us: 0,
                p99_upper_us: 0,
                max_us: 0,
            };
        }
        let percentile_upper_us = |numerator: u64| {
            let target = self.count.saturating_mul(numerator).div_ceil(100);
            let mut seen = 0;
            self.buckets
                .iter()
                .position(|count| {
                    seen += count;
                    seen >= target
                })
                .map(|bucket| TIMING_BUCKET_US.get(bucket).copied().unwrap_or(self.max_us))
                .unwrap_or(0)
        };
        TimingSummary {
            count: self.count,
            mean_us: (self.count > 0).then(|| self.total_us as f64 / self.count as f64),
            p50_upper_us: percentile_upper_us(50),
            p95_upper_us: percentile_upper_us(95),
            p99_upper_us: percentile_upper_us(99),
            max_us: self.max_us,
        }
    }
}

#[derive(Debug, Serialize)]
struct TimingSummary {
    count: u64,
    mean_us: Option<f64>,
    p50_upper_us: u64,
    p95_upper_us: u64,
    p99_upper_us: u64,
    max_us: u64,
}

struct ReplayTiming {
    input_load_seconds: f64,
    pacing_sleep_us: TimingSummary,
    schedule_lag_us: TimingSummary,
    lane_send_us: TimingSummary,
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
#[derive(Debug, Serialize)]
struct ReplaySummary {
    /// Input rows selected from the CSV before replay loops.
    input_rows: u64,
    /// Sorted rows skipped before the diagnostic window.
    skipped_rows: usize,
    /// Sorted rows selected for each loop after applying `--max-events`.
    selected_rows_per_loop: u64,
    /// Observations a lane actually submitted to ingress, including validation
    /// rejections and broker-acknowledged duplicates.
    raw_attempted: u64,
    /// Raw publications acknowledged by JetStream, duplicates included.
    raw_published: u64,
    /// Acknowledged publications deduplicated by JetStream.
    raw_duplicates: u64,
    /// Input data faults by bounded admission kind.
    raw_rejected: BTreeMap<&'static str, u64>,
    /// Requested loop count.
    loops: usize,
    /// Publish lanes used after normalising `--lanes 0` to one.
    lanes: usize,
    /// Raw stream count requested for this run.
    streams: u64,
    /// Fixed requested event rate, if fixed-rate mode was selected.
    fixed_rate_events_per_second: Option<f64>,
    /// Event-time multiplier, or `null` in flood/fixed-rate mode.
    speed_multiplier: Option<f64>,
    /// Whether this was an isolated replay journal.
    isolated: bool,
    /// Full wall time through lane drain and broker acknowledgements.
    elapsed_seconds: f64,
    /// CSV parsing and sorting time, excluded from elapsed_seconds.
    input_load_seconds: f64,
    /// Time actually spent in the pacing sleep, including timer delay.
    pacing_sleep_us: TimingSummary,
    /// Time by which the walker missed each scheduled send deadline.
    schedule_lag_us: TimingSummary,
    /// Time waiting to enqueue into a bounded publish lane.
    lane_send_us: TimingSummary,
    /// Ingress validation, publish retries, and JetStream acknowledgement time.
    publish_ack_us: TimingSummary,
}

impl ReplaySummary {
    fn new(
        args: &Args,
        input_rows: u64,
        selected_rows_per_loop: u64,
        rate: Option<f64>,
        elapsed_seconds: f64,
        timings: ReplayTiming,
        totals: LaneReport,
    ) -> Self {
        Self {
            input_rows,
            skipped_rows: args.skip_events,
            selected_rows_per_loop,
            raw_attempted: totals.attempted,
            raw_published: totals.published,
            raw_duplicates: totals.duplicates,
            raw_rejected: totals.rejected,
            loops: args.loops,
            lanes: args.lanes.max(1),
            streams: args.streams,
            fixed_rate_events_per_second: rate,
            speed_multiplier: (rate.is_none() && args.speed > 0.0).then_some(args.speed),
            isolated: args.isolated.is_some(),
            elapsed_seconds,
            input_load_seconds: timings.input_load_seconds,
            pacing_sleep_us: timings.pacing_sleep_us,
            schedule_lag_us: timings.schedule_lag_us,
            lane_send_us: timings.lane_send_us,
            publish_ack_us: totals.publish_ack.summary(),
        }
    }
}

fn report_totals(summary: &ReplaySummary) {
    if summary.raw_duplicates == 0 {
        info!(
            "replay complete: {} observation(s) published",
            summary.raw_published
        );
    } else {
        info!(
            "replay complete: {} observation(s) published ({} broker duplicate(s))",
            summary.raw_published, summary.raw_duplicates,
        );
    }

    if summary.raw_rejected.is_empty() {
        info!("no observations rejected");
        return;
    }

    let total: u64 = summary.raw_rejected.values().sum();
    warn!("{total} observation(s) rejected by validation");
    for (kind, count) in &summary.raw_rejected {
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
    fn fixed_rate_is_accepted_with_the_default_speed() {
        let args = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
            "--rate",
            "250",
        ])
        .expect("the default --speed must not conflict with --rate");
        assert_eq!(args.rate, Some(250.0));
    }

    #[test]
    fn fixed_rate_rejects_an_explicit_speed() {
        let error = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
            "--speed",
            "2",
            "--rate",
            "250",
        ])
        .expect_err("--rate and an explicit --speed are mutually exclusive");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn lane_report_merge_folds_tallies() {
        let mut totals = LaneReport::default();
        let mut a = LaneReport {
            attempted: 13,
            published: 10,
            duplicates: 2,
            ..Default::default()
        };
        *a.rejected.entry("too_old").or_default() += 3;
        let mut b = LaneReport {
            attempted: 6,
            published: 5,
            duplicates: 1,
            ..Default::default()
        };
        *b.rejected.entry("too_old").or_default() += 4;
        *b.rejected.entry("zero_vehicle").or_default() += 1;

        totals.merge(a);
        totals.merge(b);

        assert_eq!(totals.attempted, 19);
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

    #[test]
    fn timing_summary_merges_bounded_histograms() {
        let mut first = Timing::default();
        first.record(Duration::from_micros(40));
        first.record(Duration::from_micros(900));
        let mut second = Timing::default();
        second.record(Duration::from_micros(1_500));
        first.merge(second);

        let summary = first.summary();
        assert_eq!(summary.count, 3);
        assert_eq!(summary.p50_upper_us, 1_000);
        assert_eq!(summary.p95_upper_us, 2_000);
        assert_eq!(summary.max_us, 1_500);
        assert_eq!(Timing::default().summary().p95_upper_us, 0);
    }

    #[test]
    fn short_run_limit_parses() {
        let args = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
            "--max-events",
            "100000",
            "--skip-events",
            "250000",
        ])
        .expect("short diagnostic run should parse");
        assert_eq!(args.max_events, Some(100_000));
        assert_eq!(args.skip_events, 250_000);
    }
}
