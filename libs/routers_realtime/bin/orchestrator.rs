//! The partition orchestrator.
//!
//! One orchestrator process owns a disjoint slice of the vehicle partition
//! space and runs one
//! [`PartitionWorker`]
//! per owned partition, with no cross-partition sharing. This binary is only
//! wiring: connect NATS and Valkey, reconcile every plane before any worker
//! reads, recover each partition, and spawn the workers.

extern crate alloc;

use alloc::sync::Arc;
use core::num::NonZeroUsize;
use core::ops::RangeInclusive;
use core::time::Duration;
use std::collections::HashMap;

use anyhow::{Context as _, Result};
use async_nats::{ConnectOptions, ServerAddr, jetstream};
use clap::Parser;
use tracing::{error, info};

use routers_codec::osm::OsmEntryId;
use routers_realtime::bus::jetstream::{JetStreamPublisher, JetStreamSource};
use routers_realtime::lifecycle::Shutdown;
use routers_realtime::matcher::pull::RawBytes;
use routers_realtime::metrics::Metrics;
use routers_realtime::orchestrator::admission::{Admission, AdmissionConfig};
use routers_realtime::orchestrator::commit::{CommitConfig, Committer};
use routers_realtime::orchestrator::dispatch::{DispatchConfig, Dispatcher};
use routers_realtime::orchestrator::recovery::{expected_start, recover_partition};
use routers_realtime::orchestrator::scheduler::SchedulerConfig;
use routers_realtime::orchestrator::worker::{PartitionWorker, WorkerConfig, WorkerStats};
use routers_realtime::partition::PARTITIONS;
use routers_realtime::protocol::job::SolveJob;
use routers_realtime::protocol::output::CommittedOutput;
use routers_realtime::protocol::result::SolveResult;
use routers_realtime::region::catalog::Catalog;
use routers_realtime::secret::SecretUrl;
use routers_realtime::store::valkey::{ValkeyCheckpointStore, ValkeyConfig, ValkeyEndpoint};
use routers_realtime::topology::{
    JobsConfig, OutputConfig, RawConfig, ResultsConfig, ensure_job_stream, ensure_output_stream,
    ensure_raw_stream, ensure_result_stream, raw_consumer, raw_stream_index, result_consumer,
};

/// The network entry type the fleet solves against.
type E = OsmEntryId;

/// Each pod opens one raw and one result source per owned partition. The
/// broker-side `max_ack_pending` limit remains the bound on unacknowledged
/// ownership; this larger request amortises continuous-pull control traffic
/// without letting a worker claim more deliveries concurrently.
const PARTITION_SOURCE_BATCH: NonZeroUsize = NonZeroUsize::new(64).unwrap();

/// "start-end" (inclusive), or a single partition.
fn parse_partitions(s: &str) -> core::result::Result<RangeInclusive<u64>, String> {
    let (start, end) = s.split_once('-').unwrap_or((s, s));

    let start: u64 = start
        .trim()
        .parse()
        .map_err(|e| format!("bad start: {e}"))?;
    let end: u64 = end.trim().parse().map_err(|e| format!("bad end: {e}"))?;

    if start > end {
        return Err(format!("start {start} beyond end {end}"));
    }
    if end >= PARTITIONS {
        return Err(format!("partition {end} outside 0..{PARTITIONS}"));
    }
    Ok(start..=end)
}

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server.
    #[arg(short, env, long)]
    nats: SecretUrl,

    /// Valkey primaries as stable-id=URL, comma-separated. Stable ids must not
    /// change when credentials or network addresses rotate.
    #[arg(short, env, long, value_delimiter = ',')]
    valkey: Vec<ValkeyEndpoint>,

    /// Path to the region catalog (TOML): regions, graph versions, and cell coverage.
    #[arg(short, env, long)]
    catalog: std::path::PathBuf,

    /// The vehicle partitions this pod owns, as an inclusive range ("0-255"); or derive from --pod-name and --fleet.
    #[arg(short, env, long, value_parser = parse_partitions, conflicts_with_all = ["pod_name", "fleet"])]
    partitions: Option<RangeInclusive<u64>>,

    /// This pod's StatefulSet name (e.g. `orchestrator-3`); the trailing ordinal picks its slice. Pair with --fleet.
    #[arg(long, env = "POD_NAME", requires = "fleet")]
    pod_name: Option<String>,

    /// Total pods in the StatefulSet; every pod must be given the same value.
    #[arg(long, env, requires = "pod_name")]
    fleet: Option<u64>,

    /// How many raw streams the partition space divides across, fleet-wide (fixed config, not a tuning knob).
    #[arg(long, env, default_value_t = 4)]
    streams: u64,

    /// How long the raw journal retains an event — the bound on how far recovery can rewind.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "15m")]
    raw_retention: Duration,

    /// Unacknowledged raw events each partition's consumer may hold; the stream buffers, nothing is dropped.
    #[arg(long, env, default_value_t = 8)]
    raw_max_ack_pending: i64,

    /// How long the broker waits for a raw ack before redelivering.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "60s")]
    raw_ack_wait: Duration,

    /// How long the solve-result plane retains a result before it ages out.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "10m")]
    results_retention: Duration,

    /// How long committed matched output is retained for materialisers and observers to catch up.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "15m")]
    output_retention: Duration,

    /// How long an unclaimed solve job lives on the work queue (its deadline should expire first).
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "60s")]
    jobs_ttl: Duration,

    /// How long a committed checkpoint survives without a fresh commit.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "10m")]
    checkpoint_ttl: Duration,

    /// Most solve jobs that may be outstanding across the whole process.
    #[arg(long, env, default_value_t = 4096)]
    admit_global_jobs: u64,

    /// Most solve-job bytes that may be outstanding across the whole process.
    #[arg(long, env, default_value_t = 512 * 1024 * 1024)]
    admit_global_bytes: u64,

    /// Default per-region ceiling on outstanding solve jobs.
    #[arg(long, env, default_value_t = 1024)]
    admit_region_jobs: u64,

    /// Default per-region ceiling on outstanding solve-job bytes.
    #[arg(long, env, default_value_t = 128 * 1024 * 1024)]
    admit_region_bytes: u64,

    /// The largest time gap between committed observations that still continues one journey; a larger gap resets the segment.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "120s")]
    gap: Duration,

    /// The largest straight-line jump (metres) not treated as a teleport.
    #[arg(long, env, default_value_t = 2_000.0)]
    jump_distance: f64,

    /// Most early solve results parked per vehicle before further ones are dropped.
    #[arg(long, env, default_value_t = 4)]
    parked_limit: usize,

    /// How long a vehicle may sit idle before its checkpoint is evicted from the worker's local map.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "10m")]
    idle_ttl: Duration,

    /// How long shutdown waits for in-flight commits to quiesce before the worker returns.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "20s")]
    grace: Duration,

    /// How long a publish waits for the broker's ack before being retried byte-identically.
    #[arg(long, env, value_parser = humantime::parse_duration, default_value = "5s")]
    ack_timeout: Duration,
}

/// The slice of the partition space this pod owns; explicit, or derived from its
/// StatefulSet identity (contiguous ordinal blocks, last pod takes the remainder).
fn owned_partitions(args: &Args) -> Result<RangeInclusive<u64>> {
    if let Some(partitions) = &args.partitions {
        return Ok(partitions.clone());
    }

    let (Some(name), Some(fleet)) = (&args.pod_name, args.fleet) else {
        anyhow::bail!("provide --partitions, or --pod-name with --fleet");
    };
    anyhow::ensure!(
        (1..=PARTITIONS).contains(&fleet),
        "fleet size {fleet} outside 1..={PARTITIONS}"
    );

    let ordinal: u64 = name
        .rsplit('-')
        .next()
        .unwrap_or_default()
        .parse()
        .with_context(|| format!("pod name {name:?} carries no trailing ordinal"))?;
    anyhow::ensure!(
        ordinal < fleet,
        "ordinal {ordinal} outside fleet of {fleet}"
    );

    let chunk = PARTITIONS / fleet;
    let start = ordinal * chunk;
    let end = if ordinal == fleet - 1 {
        PARTITIONS - 1
    } else {
        (ordinal + 1) * chunk - 1
    };
    Ok(start..=end)
}

/// The static per-worker configuration derived from the CLI args.
fn worker_config(args: &Args, partition: u16) -> WorkerConfig {
    WorkerConfig {
        scheduler: SchedulerConfig {
            idle_ttl: args.idle_ttl,
            ..SchedulerConfig::default()
        },
        parked_limit: args.parked_limit,
        dispatch: dispatch_config(args),
        commit: CommitConfig::default(),
        grace: args.grace,
        ..WorkerConfig::new(partition)
    }
}

/// The dispatcher's continuity/publish knobs from the CLI args.
fn dispatch_config(args: &Args) -> DispatchConfig {
    DispatchConfig {
        gap: args.gap,
        jump_distance_m: args.jump_distance,
        ..DispatchConfig::default()
    }
}

/// The admission controller's ceilings from the CLI args.
fn admission_config(args: &Args) -> AdmissionConfig {
    AdmissionConfig {
        global_jobs: args.admit_global_jobs,
        global_bytes: args.admit_global_bytes,
        region_jobs: args.admit_region_jobs,
        region_bytes: args.admit_region_bytes,
        overrides: HashMap::new(),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let _telemetry = routers_realtime::telemetry::init("routers-orchestrator");
    // Built after telemetry so instruments bind to the installed meter.
    let metrics = Metrics::new();

    let args = Args::parse();
    info!("orchestrator started: {args:?}");

    let shutdown = Shutdown::from_signals();

    // The context is cloned per publisher, sharing one multiplexed connection.
    let nats_url =
        ServerAddr::from_url(args.nats.connection_url()).context("could not create NATS url")?;
    let client = ConnectOptions::new()
        .name("OrchestratorService")
        .connect(nats_url)
        .await
        .context("could not connect to NATS")?;
    let context = jetstream::new(client);

    let catalog = Arc::new(
        Catalog::load(&args.catalog)
            .with_context(|| format!("could not load catalog from {}", args.catalog.display()))?,
    );

    // Reconcile every plane before any worker reads.
    let raw_cfg = RawConfig {
        streams: args.streams,
        max_age: args.raw_retention,
        max_ack_pending: args.raw_max_ack_pending,
        ack_wait: args.raw_ack_wait,
    };
    let mut raw_streams = Vec::with_capacity(args.streams as usize);
    for index in 0..args.streams {
        raw_streams.push(ensure_raw_stream(&context, index, args.streams, &raw_cfg).await?);
    }

    let results_cfg = ResultsConfig {
        max_age: args.results_retention,
        ..ResultsConfig::default()
    };
    let result_stream = ensure_result_stream(&context, &results_cfg).await?;

    let output_cfg = OutputConfig {
        max_age: args.output_retention,
    };
    ensure_output_stream(&context, &output_cfg).await?;

    let jobs_cfg = JobsConfig {
        max_age: args.jobs_ttl,
        ..JobsConfig::default()
    };
    for region in &catalog.regions {
        ensure_job_stream(&context, &region.id, &jobs_cfg).await?;
    }

    let store = ValkeyCheckpointStore::connect(ValkeyConfig {
        checkpoint_ttl: args.checkpoint_ttl,
        ..ValkeyConfig::new(args.valkey.clone())
    })
    .await
    .context("could not connect to the Valkey checkpoint store")?;
    let admission = Admission::new(
        admission_config(&args),
        catalog.regions.iter().map(|r| &r.id),
    );

    // Register the process-scoped admission gauges exactly once (not per worker).
    let _admission_gauges = admission.register_gauges(&metrics);

    let job_publisher = JetStreamPublisher::<SolveJob<E>>::new(context.clone(), args.ack_timeout);
    let output_publisher =
        JetStreamPublisher::<CommittedOutput<E>>::new(context.clone(), args.ack_timeout);

    let owned = owned_partitions(&args)?;
    info!(
        "orchestrating partitions {:?} across {} raw stream(s)",
        owned, args.streams
    );

    let mut handles = Vec::new();
    for partition in owned.clone() {
        let partition = partition as u16;

        // Recover before creating the raw consumer: it reports the frontier to resume just past.
        let committer = Committer::new(
            store.clone(),
            output_publisher.clone(),
            CommitConfig::default(),
        );
        let report = recover_partition(&store, &committer, partition)
            .await
            .with_context(|| format!("could not recover partition {partition}"))?;
        info!(
            partition,
            frontier = ?report.frontier,
            prepared_found = report.prepared_found,
            prepared_finished = report.prepared_finished,
            prepared_failed = report.prepared_failed.len(),
            "partition recovered"
        );

        // Resume at `frontier + 1` (or stream head); `raw_consumer` verifies the deliver policy matches.
        let raw_stream =
            &raw_streams[raw_stream_index(u64::from(partition), args.streams) as usize];
        let raw = raw_consumer(
            raw_stream,
            u64::from(partition),
            &raw_cfg,
            expected_start(report.frontier),
        )
        .await?;
        let raw = JetStreamSource::<RawBytes>::from_consumer(&raw, PARTITION_SOURCE_BATCH)
            .await
            .with_context(|| format!("could not open raw source for partition {partition}"))?;

        let results = result_consumer(&result_stream, u64::from(partition), &results_cfg).await?;
        let results =
            JetStreamSource::<SolveResult<E>>::from_consumer(&results, PARTITION_SOURCE_BATCH)
                .await
                .with_context(|| {
                    format!("could not open result source for partition {partition}")
                })?;

        let dispatcher = Dispatcher::new(job_publisher.clone(), dispatch_config(&args));
        let worker = PartitionWorker::new(
            worker_config(&args, partition),
            catalog.clone(),
            admission.clone(),
            store.clone(),
            dispatcher,
            committer,
            raw,
            results,
            &report,
            shutdown.clone(),
        )
        .with_metrics(metrics.clone());
        handles.push((partition, tokio::spawn(worker.run())));
    }

    // A failed worker exits the process non-zero, but only after every peer drains.
    let mut failed = false;
    for (partition, handle) in handles {
        match handle.await {
            Ok(Ok(stats)) => log_stats(partition, &stats),
            Ok(Err(err)) => {
                failed = true;
                error!(partition, "partition worker failed: {err:#}");
            }
            Err(join_err) => {
                failed = true;
                error!(partition, "partition worker panicked: {join_err}");
            }
        }
    }

    anyhow::ensure!(!failed, "one or more partition workers failed");
    Ok(())
}

/// Log one partition worker's exit summary. Every field is a bounded, label-free
/// count, so it is safe to emit at `info`.
fn log_stats(partition: u16, stats: &WorkerStats) {
    info!(
        partition,
        observed = stats.observed,
        queued = stats.queued,
        dispatched = stats.dispatched,
        accepted = stats.accepted,
        committed = stats.committed,
        terminal = stats.terminal,
        resets = stats.resets,
        conflicts = stats.conflicts,
        poison = stats.poison,
        frontier = stats.frontier,
        "partition worker stopped"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(extra: &[&str]) -> Args {
        let base = [
            "orchestrator",
            "--nats",
            "nats://localhost",
            "--valkey",
            "primary=redis://localhost",
            "--catalog",
            "/tmp/catalog.toml",
        ];
        Args::parse_from(base.iter().copied().chain(extra.iter().copied()))
    }

    /// Every fleet size slices the space disjointly and completely, ordinal
    /// blocks in order, the last pod absorbing the remainder.
    #[test]
    fn fleet_slices_cover_the_space_disjointly() {
        for fleet in [1u64, 3, 4, 7, 16] {
            let mut next = 0;
            for ordinal in 0..fleet {
                let name = format!("orchestrator-{ordinal}");
                let range =
                    owned_partitions(&args(&["--pod-name", &name, "--fleet", &fleet.to_string()]))
                        .unwrap();

                assert_eq!(*range.start(), next, "fleet {fleet} ordinal {ordinal}");
                next = range.end() + 1;
            }
            assert_eq!(next, PARTITIONS, "fleet {fleet} must cover the space");
        }
    }

    #[test]
    fn partition_ranges_parse_and_validate() {
        assert_eq!(parse_partitions("0-255").unwrap(), 0..=255);
        assert_eq!(parse_partitions("7").unwrap(), 7..=7);
        assert!(parse_partitions("5-4").is_err());
        assert!(parse_partitions("0-1024").is_err());
    }

    #[test]
    fn fleet_rejects_a_nameless_or_oversized_ordinal() {
        assert!(owned_partitions(&args(&["--pod-name", "orchestrator", "--fleet", "4"])).is_err());
        assert!(
            owned_partitions(&args(&["--pod-name", "orchestrator-4", "--fleet", "4"])).is_err()
        );
        assert!(owned_partitions(&args(&[])).is_err());
    }

    #[test]
    fn explicit_partitions_win_over_identity() {
        let parsed = args(&["--partitions", "10-20"]);
        assert_eq!(owned_partitions(&parsed).unwrap(), 10..=20);
    }

    #[test]
    fn valkey_urls_split_on_commas() {
        // A comma-separated value splits into one URL per entry; this second
        // `--valkey` occurrence adds two to the base helper's single URL.
        let parsed = args(&["--valkey", "a=redis://a:6379,b=redis://b:6379"]);
        assert_eq!(parsed.valkey.len(), 3);
    }

    /// The config derivations thread the CLI knobs into the right nested config,
    /// and keep the worker's mirror of the dispatch knobs in step with the
    /// dispatcher the worker is handed.
    #[test]
    fn config_derivation_threads_the_knobs() {
        let parsed = args(&[
            "--gap",
            "30s",
            "--jump-distance",
            "1500",
            "--parked-limit",
            "9",
            "--idle-ttl",
            "3m",
            "--grace",
            "7s",
            "--admit-global-jobs",
            "10",
        ]);

        let dispatch = dispatch_config(&parsed);
        assert_eq!(dispatch.gap, Duration::from_secs(30));
        assert_eq!(dispatch.jump_distance_m, 1500.0);

        let worker = worker_config(&parsed, 5);
        assert_eq!(worker.partition, 5);
        assert_eq!(worker.parked_limit, 9);
        assert_eq!(worker.scheduler.idle_ttl, Duration::from_secs(180));
        assert_eq!(worker.grace, Duration::from_secs(7));
        // The worker's mirror of the dispatch knobs matches the dispatcher's.
        assert_eq!(worker.dispatch, dispatch);

        let admission = admission_config(&parsed);
        assert_eq!(admission.global_jobs, 10);
        assert_eq!(admission.region_jobs, 1024);
    }
}
