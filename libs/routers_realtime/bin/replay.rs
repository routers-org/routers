/// Loads and sorts the full dataset, then walks events in chronological
/// order, validating and publishing each through [`Ingress`]. By default it
/// feeds the live raw journal; `--isolated <run>` feeds a private
/// `replay.<run>.` journal instead. Publishing fans out across `--lanes` tasks,
/// each vehicle pinned to one lane so its send order (load-bearing: revisions
/// are stream sequences) is preserved, while lanes overlap their round-trips.
extern crate alloc;

use alloc::collections::{BTreeMap, BinaryHeap};
use core::cmp::Reverse;
use core::fmt::Write;
use core::time::Duration;
use std::collections::{HashMap, HashSet, VecDeque};
use std::future::Future;
use std::path::PathBuf;

use anyhow::Context;
use async_nats::{ConnectOptions, ServerAddr, jetstream};
use clap::Parser;
use fnv_rs::{Fnv64, FnvHasher};
use futures::{StreamExt, future::BoxFuture, stream::FuturesUnordered};
use geo::Point;
use indicatif::{ProgressBar, ProgressDrawTarget, ProgressState, ProgressStyle};
use indicatif_log_bridge::LogWrapper;
use itertools::izip;
use log::{debug, info, warn};
use polars::prelude::*;
use routers_realtime::{
    bus,
    event::{Payload, VehicleId},
    ingress::{Ingress, IngressError, PublishAck},
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
const LANE_PENDING_CAPACITY: usize = 1024;
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
    #[arg(long, env, default_value_t = 128)]
    lanes: usize,

    /// Maximum concurrent publishes per lane, with at most one outstanding per vehicle.
    #[arg(long, env, default_value_t = 4)]
    lane_in_flight: usize,

    /// Replicate each source vehicle into this many independent synthetic vehicles.
    /// The first copy keeps its original ID; others are deterministic hashes.
    /// Use this to raise vehicle concurrency without changing event order.
    #[arg(long, env, default_value_t = 1)]
    vehicle_copies: usize,

    /// Spread copies across the selected time window instead of replaying them
    /// in lockstep: copy `k` is shifted by `k/N` of the window and wraps at its
    /// end, its post-wrap tail becoming a separate synthetic vehicle. Offered
    /// load is then flat and every vehicle keeps its source cadence (scaled
    /// only by `--speed`), so `--speed 1` with many copies approximates a large
    /// live fleet. Payload timestamps are shifted with the schedule.
    #[arg(long, env, conflicts_with = "rate")]
    copy_spread: bool,

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
    anyhow::ensure!(
        (1..=MAX_VEHICLE_COPIES).contains(&args.vehicle_copies),
        "--vehicle-copies must be between 1 and {MAX_VEHICLE_COPIES}"
    );
    // A spread copy's wrapped tail is its own vehicle, so it needs its own ids.
    let id_copies = if args.copy_spread {
        args.vehicle_copies * 2
    } else {
        args.vehicle_copies
    };
    validate_vehicle_copies(&df, id_copies)?;
    let selected_events = selected_rows
        .checked_mul(args.vehicle_copies)
        .context("selected row count overflows with --vehicle-copies")?;
    let pb = ProgressBar::new(selected_events as u64);
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
    } else if args.copy_spread {
        pg.set_message(format!(
            "[spread-mode] speed={speed}x copies={} (per-vehicle cadence preserved)",
            args.vehicle_copies
        ));
    } else if flood {
        pg.set_message("[flood-mode] speed=∞x".to_string());
    } else {
        pg.set_message(format!(
            "[walk-mode] speed={speed}x (walltime={timespan_s:.1} s, realtime={realtime_s:.1} s)"
        ));
    }

    // At least one lane; `--lanes 0` is a no-op knob, not an empty run.
    let lanes = args.lanes.max(1);
    anyhow::ensure!(args.lane_in_flight > 0, "--lane-in-flight must be positive");

    // Each lane owns an `Ingress` clone and drains a bounded channel; a broker failure surfaces as the lane returning `Err`.
    let mut senders: Vec<mpsc::Sender<Payload>> = Vec::with_capacity(lanes);
    let mut set: JoinSet<anyhow::Result<LaneReport>> = JoinSet::new();
    for _ in 0..lanes {
        let (tx, rx) = mpsc::channel::<Payload>(LANE_CHANNEL_CAPACITY);
        senders.push(tx);
        let publisher = ingress.clone();
        set.spawn(run_lane(
            rx,
            args.lane_in_flight,
            args.streams,
            move |payload| {
                let publisher = publisher.clone();
                async move { publisher.publish(&payload, bus::wallclock()).await }
            },
        ));
    }
    // The walker no longer publishes; drop its redundant clone.
    drop(ingress);

    // A lane that exits early (broker failure) is captured here.
    let mut early: Option<Result<anyhow::Result<LaneReport>, JoinError>> = None;
    let replay_started = Instant::now();
    let mut pacing_sleep = Timing::default();
    let mut schedule_lag = Timing::default();
    let mut lane_send = Timing::default();
    let mut offered = 0_u64;
    'walk: for iteration in 0..args.loops {
        pg.reset();

        debug!("loop {iteration}/{0}", args.loops);
        let rows = rows_of(&df).context("could not deserialize rows from dataframe")?;

        let selected = rows.skip(args.skip_events).take(selected_rows);
        let copies = args.vehicle_copies;
        let events: Box<dyn Iterator<Item = (Duration, Payload)> + '_> = if args.copy_spread {
            Box::new(
                SpreadSchedule::new(selected.collect(), copies)
                    .map(move |(shifted, payload)| (shifted.div_f64(speed), payload)),
            )
        } else {
            let mut first_time = None;
            Box::new(
                selected
                    .enumerate()
                    .flat_map(move |(offset_index, (time, payload))| {
                        let first = *first_time.get_or_insert(time);
                        (0..copies).map(move |copy| {
                            let offset = rate.map_or_else(
                                || Duration::from_micros(time - first).div_f64(speed),
                                |rate| {
                                    Duration::from_secs_f64(
                                        (offset_index * copies + copy) as f64 / rate,
                                    )
                                },
                            );
                            let copied = Payload {
                                vehicle_id: copied_vehicle(payload.vehicle_id, copy),
                                timestamp: payload.timestamp,
                                point: payload.point,
                            };
                            (offset, copied)
                        })
                    }),
            )
        };

        let start = Instant::now();
        for (offset, copied) in events {
            pg.inc(1);
            if !flood {
                let deadline = start + offset;
                let sleep_started = Instant::now();
                tokio::time::sleep_until(deadline).await;
                pacing_sleep.record(sleep_started.elapsed());
                schedule_lag.record(Instant::now().saturating_duration_since(deadline));
            }

            let lane = lane_of(copied.vehicle_id, lanes);
            let send_started = Instant::now();
            let sent = senders[lane].send(copied).await;
            lane_send.record(send_started.elapsed());
            if sent.is_err() {
                break 'walk;
            }
            offered += 1;

            if let Some(joined) = set.try_join_next() {
                early = Some(joined);
                break 'walk;
            }
        }
        pg.finish();
    }

    let offer_seconds = replay_started.elapsed().as_secs_f64();
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
        offered,
        offer_seconds,
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

/// The most synthetic copies of each source vehicle; a spread copy uses twice
/// as many ids (its wrapped tail is a separate vehicle).
const MAX_VEHICLE_COPIES: usize = 4_096;

/// A lazily merged, time-ordered schedule of `copies` time-shifted replicas of
/// one sorted row window.
///
/// Copy `k` is shifted by `k·W/copies` within the window width `W` and wraps
/// at `W`. Rows that wrap become a distinct synthetic vehicle (copy index
/// `copies + k`), so every emitted vehicle's observations stay contiguous and
/// time-ordered. Each emitted payload's timestamp is `first + shifted`, the
/// same instant its schedule offset represents.
struct SpreadSchedule {
    rows: Vec<(u64, Payload)>,
    first: u64,
    window: u64,
    copies: usize,
    /// Per copy: the first unwrapped row index and how many rows it has emitted.
    cursors: Vec<(usize, usize)>,
    heap: BinaryHeap<Reverse<(u64, usize)>>,
}

impl SpreadSchedule {
    fn new(rows: Vec<(u64, Payload)>, copies: usize) -> Self {
        let first = rows.first().map_or(0, |(time, _)| *time);
        let last = rows.last().map_or(0, |(time, _)| *time);
        // One microsecond past the last row, so the last row never wraps onto the first.
        let window = last - first + 1;
        let mut schedule = Self {
            rows,
            first,
            window,
            copies,
            cursors: Vec::with_capacity(copies),
            heap: BinaryHeap::with_capacity(copies),
        };
        for copy in 0..copies {
            let shift = schedule.shift(copy);
            let split = schedule
                .rows
                .partition_point(|(time, _)| time - first + shift < window);
            schedule.cursors.push((split, 0));
            if let Some(at) = schedule.shifted(copy, 0) {
                schedule.heap.push(Reverse((at, copy)));
            }
        }
        schedule
    }

    fn shift(&self, copy: usize) -> u64 {
        (u128::from(self.window) * copy as u128 / self.copies as u128) as u64
    }

    /// The row a copy emits at position `emitted`, and whether it wrapped.
    fn row(&self, copy: usize, emitted: usize) -> Option<(usize, bool)> {
        let len = self.rows.len();
        if emitted >= len {
            return None;
        }
        let (split, _) = self.cursors[copy];
        // Wrapped rows (`split..`) land at the window's start, so they lead.
        let index = (split + emitted) % len;
        Some((index, index >= split))
    }

    fn shifted(&self, copy: usize, emitted: usize) -> Option<u64> {
        let (index, wrapped) = self.row(copy, emitted)?;
        let relative = self.rows[index].0 - self.first + self.shift(copy);
        Some(if wrapped {
            relative - self.window
        } else {
            relative
        })
    }
}

impl Iterator for SpreadSchedule {
    type Item = (Duration, Payload);

    fn next(&mut self) -> Option<Self::Item> {
        let Reverse((at, copy)) = self.heap.pop()?;
        let (_, emitted) = self.cursors[copy];
        let (index, wrapped) = self.row(copy, emitted).expect("a queued copy has a row");
        self.cursors[copy].1 += 1;
        if let Some(next) = self.shifted(copy, emitted + 1) {
            self.heap.push(Reverse((next, copy)));
        }
        let source = &self.rows[index].1;
        let identity = if wrapped { self.copies + copy } else { copy };
        let micros = i64::try_from(self.first + at).unwrap_or(i64::MAX);
        let payload = Payload {
            vehicle_id: copied_vehicle(source.vehicle_id, identity),
            timestamp: chrono::DateTime::from_timestamp_micros(micros).unwrap_or_default(),
            point: source.point,
        };
        Some((Duration::from_micros(at), payload))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.rows.len() * self.copies
            - self
                .cursors
                .iter()
                .map(|(_, emitted)| emitted)
                .sum::<usize>();
        (remaining, Some(remaining))
    }
}

fn copied_vehicle(original: VehicleId, copy: usize) -> VehicleId {
    if copy == 0 {
        return original;
    }
    let mut input = [0_u8; 16];
    input[..8].copy_from_slice(&original.0.to_be_bytes());
    input[8..].copy_from_slice(&(copy as u64).to_be_bytes());
    let digest: [u8; 8] = Fnv64::hash(input)
        .as_bytes()
        .try_into()
        .expect("FNV-64 bytes");
    VehicleId(u64::from_be_bytes(digest))
}

fn validate_vehicle_copies(df: &DataFrame, copies: usize) -> anyhow::Result<()> {
    if copies == 1 {
        return Ok(());
    }
    let originals: HashSet<VehicleId> = df
        .column(VEHICLE_ID_COL)?
        .str()?
        .into_iter()
        .flatten()
        .map(|value| {
            let digest: [u8; 8] = Fnv64::hash(value)
                .as_bytes()
                .try_into()
                .expect("FNV-64 bytes");
            VehicleId(u64::from_be_bytes(digest))
        })
        .collect();
    let mut mapped = HashSet::with_capacity(originals.len().saturating_mul(copies));
    for copy in 0..copies {
        for &original in &originals {
            let synthetic = copied_vehicle(original, copy);
            anyhow::ensure!(
                synthetic.0 != 0 && mapped.insert(synthetic),
                "--vehicle-copies produced a zero or colliding vehicle ID"
            );
        }
    }
    Ok(())
}

type PublishFuture =
    BoxFuture<'static, (VehicleId, u64, Result<PublishAck, IngressError>, Duration)>;

/// Publish different vehicles concurrently, but await each vehicle's ack before
/// starting its next observation, preserving stream-sequence revisions.
async fn run_lane<F, Fut>(
    mut rx: mpsc::Receiver<Payload>,
    max_in_flight: usize,
    streams: u64,
    publish: F,
) -> anyhow::Result<LaneReport>
where
    F: Fn(Payload) -> Fut + Clone + Send + 'static,
    Fut: Future<Output = Result<PublishAck, IngressError>> + Send + 'static,
{
    let mut report = LaneReport::default();
    let mut waiting: HashMap<VehicleId, VecDeque<Payload>> = HashMap::new();
    let mut ready = VecDeque::new();
    let mut active = HashSet::new();
    let mut publishes: FuturesUnordered<PublishFuture> = FuturesUnordered::new();
    let mut buffered = 0_usize;
    let mut open = true;

    loop {
        while publishes.len() < max_in_flight {
            let Some(vehicle) = ready.pop_front() else {
                break;
            };
            let queue = waiting
                .get_mut(&vehicle)
                .expect("ready vehicle has a queue");
            let payload = queue.pop_front().expect("ready vehicle has a payload");
            if queue.is_empty() {
                waiting.remove(&vehicle);
            }
            active.insert(vehicle);
            let publisher = publish.clone();
            let stream = topology::raw_stream_index(partition::partition_of(vehicle), streams);
            publishes.push(Box::pin(async move {
                let started = Instant::now();
                let result = publisher(payload).await;
                (vehicle, stream, result, started.elapsed())
            }));
        }

        if !open && buffered == 0 {
            break;
        }

        tokio::select! {
            payload = rx.recv(), if open && buffered < LANE_PENDING_CAPACITY => {
                match payload {
                    Some(payload) => {
                        report.attempted += 1;
                        buffered += 1;
                        let vehicle = payload.vehicle_id;
                        let queue = waiting.entry(vehicle).or_default();
                        if queue.is_empty() && !active.contains(&vehicle) {
                            ready.push_back(vehicle);
                        }
                        queue.push_back(payload);
                    }
                    None => open = false,
                }
            }
            Some((vehicle, stream, result, elapsed)) = publishes.next(), if !publishes.is_empty() => {
                buffered -= 1;
                active.remove(&vehicle);
                if waiting.contains_key(&vehicle) {
                    ready.push_back(vehicle);
                }
                report.publish_ack.record(elapsed);
                report.stream_acks.entry(stream).or_default().record(elapsed);
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
    stream_acks: BTreeMap<u64, Timing>,
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
        for (stream, timing) in other.stream_acks {
            self.stream_acks.entry(stream).or_default().merge(timing);
        }
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
    lane_in_flight: usize,
    /// Number of independent synthetic vehicles per input vehicle.
    vehicle_copies: usize,
    copy_spread: bool,
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
    /// Time until the walker handed off its last observation, before lane drain.
    offer_seconds: f64,
    offered_events: u64,
    offered_events_per_second: f64,
    acknowledged_events_per_second: f64,
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
    raw_stream_publish_ack_us: BTreeMap<u64, TimingSummary>,
}

impl ReplaySummary {
    fn new(
        args: &Args,
        input_rows: u64,
        selected_rows_per_loop: u64,
        rate: Option<f64>,
        offered: u64,
        offer_seconds: f64,
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
            lane_in_flight: args.lane_in_flight,
            vehicle_copies: args.vehicle_copies,
            copy_spread: args.copy_spread,
            streams: args.streams,
            fixed_rate_events_per_second: rate,
            speed_multiplier: (rate.is_none() && args.speed > 0.0).then_some(args.speed),
            isolated: args.isolated.is_some(),
            elapsed_seconds,
            offer_seconds,
            offered_events: offered,
            offered_events_per_second: offered as f64 / offer_seconds,
            acknowledged_events_per_second: totals.published as f64 / elapsed_seconds,
            input_load_seconds: timings.input_load_seconds,
            pacing_sleep_us: timings.pacing_sleep_us,
            schedule_lag_us: timings.schedule_lag_us,
            lane_send_us: timings.lane_send_us,
            publish_ack_us: totals.publish_ack.summary(),
            raw_stream_publish_ack_us: totals
                .stream_acks
                .iter()
                .map(|(&stream, timing)| (stream, timing.summary()))
                .collect(),
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
    use std::sync::{Arc, Mutex};

    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn lane_overlaps_vehicles_but_preserves_each_vehicles_order() {
        let (tx, rx) = mpsc::channel(4);
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let (release_tx, release_rx) = oneshot::channel();
        let gate = Arc::new(Mutex::new(Some(release_rx)));
        let publish = move |payload: Payload| {
            let started_tx = started_tx.clone();
            let gate = Arc::clone(&gate);
            async move {
                let vehicle = payload.vehicle_id.0;
                let sequence = payload.timestamp.timestamp_micros();
                started_tx
                    .send((vehicle, sequence))
                    .expect("test receiver open");
                if (vehicle, sequence) == (1, 1) {
                    let release = gate.lock().expect("gate lock").take().expect("first send");
                    release.await.expect("release first send");
                }
                Ok(PublishAck {
                    sequence: 1,
                    duplicate: false,
                })
            }
        };
        let lane = tokio::spawn(run_lane(rx, 2, 4, publish));
        let payload = |vehicle, micros| Payload {
            vehicle_id: VehicleId(vehicle),
            timestamp: chrono::DateTime::from_timestamp_micros(micros).expect("valid timestamp"),
            point: Point::new(151.0, -33.0),
        };
        tx.send(payload(1, 1)).await.expect("lane open");
        tx.send(payload(1, 2)).await.expect("lane open");
        tx.send(payload(2, 1)).await.expect("lane open");

        let first = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("first publish started");
        let second = tokio::time::timeout(Duration::from_secs(1), started_rx.recv())
            .await
            .expect("second publish started");
        assert_eq!(first, Some((1, 1)));
        assert_eq!(second, Some((2, 1)));
        assert!(started_rx.try_recv().is_err());

        release_tx.send(()).expect("first publish waiting");
        assert_eq!(started_rx.recv().await, Some((1, 2)));
        drop(tx);
        let report = lane.await.expect("lane task").expect("lane result");
        assert_eq!(report.attempted, 3);
        assert_eq!(report.published, 3);
        assert_eq!(
            report
                .stream_acks
                .values()
                .map(|timing| timing.count)
                .sum::<u64>(),
            3
        );
    }

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
    fn synthetic_vehicle_copies_are_stable_and_distinct() {
        let originals = [VehicleId(1), VehicleId(42), VehicleId(u64::MAX)];
        let mut seen = HashSet::new();
        for copy in 0..8 {
            for original in originals {
                let copied = copied_vehicle(original, copy);
                assert_eq!(copied, copied_vehicle(original, copy));
                if copy == 0 {
                    assert_eq!(copied, original);
                }
                assert_ne!(copied.0, 0);
                assert!(seen.insert(copied));
            }
        }
    }

    fn row(vehicle: u64, time: u64) -> (u64, Payload) {
        (
            time,
            Payload {
                vehicle_id: VehicleId(vehicle),
                timestamp: chrono::DateTime::from_timestamp_micros(time as i64).unwrap(),
                point: Point::new(151.0, -33.0),
            },
        )
    }

    #[test]
    fn spread_schedule_is_ordered_complete_and_flat() {
        // Two vehicles, one event per 10 s over a 100 s window.
        let rows: Vec<_> = (0..10u64)
            .map(|i| row(1 + i % 2, 1_000_000_000 + i * 10_000_000))
            .collect();
        let copies = 4;
        let len = rows.len();
        let events: Vec<_> = SpreadSchedule::new(rows, copies).collect();
        assert_eq!(events.len(), len * copies);
        assert!(
            events.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "the merged schedule is time-ordered"
        );
        // A flat offer: each quarter of the window carries a quarter of events.
        let window = events.last().unwrap().0.as_micros() as f64;
        let early = events
            .iter()
            .filter(|(at, _)| (at.as_micros() as f64) < window / 4.0)
            .count();
        assert!(
            (8..=12).contains(&early),
            "{early} of 40 events in the first quarter"
        );
    }

    #[test]
    fn spread_vehicles_keep_monotone_timestamps_and_source_cadence() {
        let rows: Vec<_> = (0..10u64)
            .map(|i| row(1, 1_000_000_000 + i * 10_000_000))
            .collect();
        let mut by_vehicle: HashMap<VehicleId, Vec<(Duration, i64)>> = HashMap::new();
        for (at, payload) in SpreadSchedule::new(rows, 3) {
            by_vehicle
                .entry(payload.vehicle_id)
                .or_default()
                .push((at, payload.timestamp.timestamp_micros()));
        }
        // Copy 0 stays whole; copies 1 and 2 each split at the wrap.
        assert_eq!(by_vehicle.len(), 5);
        assert_eq!(by_vehicle[&VehicleId(1)].len(), 10, "copy 0 keeps its id");
        for events in by_vehicle.values() {
            for pair in events.windows(2) {
                let (a, b) = (pair[0], pair[1]);
                assert!(b.1 > a.1, "timestamps advance within a vehicle");
                assert_eq!(b.1 - a.1, 10_000_000, "source cadence is preserved");
                assert_eq!((b.0 - a.0).as_micros() as i64, b.1 - a.1);
            }
        }
    }

    #[test]
    fn copy_spread_rejects_fixed_rate() {
        let error = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
            "--copy-spread",
            "--rate",
            "250",
        ])
        .expect_err("--copy-spread paces by timestamps");
        assert_eq!(error.kind(), clap::error::ErrorKind::ArgumentConflict);
    }

    #[test]
    fn vehicle_copies_arg_is_honoured() {
        let args = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
            "--vehicle-copies",
            "4",
        ])
        .expect("vehicle copies should parse");
        assert_eq!(args.vehicle_copies, 4);
    }

    #[test]
    fn lanes_default_is_one_hundred_twenty_eight() {
        let args = Args::try_parse_from([
            "replay",
            "--file",
            "in.csv",
            "--nats",
            "nats://localhost:4222",
        ])
        .expect("minimal args should parse");
        assert_eq!(args.lanes, 128);
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
