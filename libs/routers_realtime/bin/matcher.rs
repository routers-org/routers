//! The regional matcher worker binary.
//!
//! One matcher serves exactly one region: it loads that region's pinned graph
//! before pulling any job, then pulls solve jobs only as fast as it has CPU
//! slots, solves each, and publishes the result before acknowledging the job so
//! a crash never loses work. This binary is only the wiring around the reusable
//! pieces in [`routers_realtime::matcher`].

// A binary crate has no `extern crate alloc`, so `std::sync::Arc` is the only spelling.
#![allow(clippy::std_instead_of_alloc)]

use core::time::Duration;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use clap::Parser;
use tracing::info;
use url::Url;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::Metadata;

use routers_realtime::bus::jetstream::{JetStreamConsumer, JetStreamPublisher};
use routers_realtime::lifecycle::{Drain, Readiness, ReadyState, Shutdown};
use routers_realtime::matcher::bootstrap::{BootstrapConfig, bootstrap};
use routers_realtime::matcher::engine::Engine;
use routers_realtime::matcher::publish::{PublishConfig, ResultPublisher};
use routers_realtime::matcher::pull::{PullConfig, PullLoop, RawBytes};
use routers_realtime::matcher::validate::ValidateConfig;
use routers_realtime::metrics::Metrics;
use routers_realtime::protocol::ids::{IdError, RegionId};
use routers_realtime::protocol::result::SolveResult;
use routers_realtime::topology::jobs::{JobsConfig, job_consumer, open_job_stream};

/// The network entry type this region's graph is keyed by.
type E = OsmEntryId;

/// How long a result publish waits for the broker's ack before being retried idempotently.
const PUBLISH_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one fetch waits for a batch to fill; short, so a lone job is not held back.
const FETCH_WAIT: Duration = Duration::from_millis(100);

/// The default solve-slot count: one per available core, since a solve is
/// CPU-bound. Falls back to a single slot when parallelism is unknown.
fn default_slots() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Validate a `--region` value as a NATS-safe [`RegionId`].
fn parse_region(value: &str) -> Result<RegionId, IdError> {
    RegionId::new(value)
}

/// Command-line configuration for a regional matcher.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// URL of the NATS server to connect to.
    #[arg(long)]
    nats: Url,

    /// Path to the region catalog TOML (mounted read-only).
    #[arg(long)]
    catalog: PathBuf,

    /// The region this matcher serves; must exist in the catalog.
    #[arg(long, value_parser = parse_region)]
    region: RegionId,

    /// Directory holding the `manifest.json` and the region's `.shard.rt`
    /// bundles.
    #[arg(long)]
    shard_dir: PathBuf,

    /// The most solves to run concurrently (and jobs claimed-but-unanswered).
    /// Defaults to the available parallelism.
    #[arg(long, default_value_t = default_slots())]
    slots: usize,

    /// Optional marker file created once the graph is ready and removed on any
    /// other state, so an exec probe can `test -f <path>`.
    #[arg(long)]
    ready_file: Option<PathBuf>,

    /// How long to wait for in-flight solves to finish on shutdown before
    /// abandoning them to redelivery.
    #[arg(long, default_value = "20s", value_parser = humantime::parse_duration)]
    grace: Duration,

    /// The largest wire buffer a job may decode to; enforced before decode so a
    /// hostile length prefix cannot force an unbounded allocation.
    #[arg(long, default_value_t = ValidateConfig::DEFAULT_MAX_DECODED_BYTES)]
    max_decoded_bytes: usize,

    /// Override the candidate generator's search distance; `None` keeps the
    /// generator's own default.
    #[arg(long)]
    search_distance: Option<f64>,

    /// Candidates kept per observation, best emission first; bounds job and result size. 0 keeps all.
    #[arg(long, default_value_t = 16)]
    max_candidates: usize,

    /// Layers carried between solves; older layers are finalised at the cut. 0 carries all.
    #[arg(long, default_value_t = 32)]
    window_layers: usize,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _telemetry = routers_realtime::telemetry::init("routers-matcher");
    // Built after telemetry so the instruments bind to the installed meter.
    let metrics = Metrics::new();

    let args = Args::parse();
    info!(?args, "matcher starting");

    let shutdown = Shutdown::from_signals();

    let (readiness, _watcher) = Readiness::new();
    let readiness = match args.ready_file.clone() {
        Some(path) => readiness.with_ready_file(path),
        None => readiness,
    };

    // On failure bootstrap has already published `Failed`; the `?` exits non-zero so an unready process never pulls.
    let boot = BootstrapConfig {
        catalog: args.catalog.clone(),
        region: args.region.clone(),
        shard_dir: args.shard_dir.clone(),
    };
    let loaded = bootstrap(&boot, &readiness)
        .await
        .context("graph bootstrap failed")?;

    // Flip readiness to `Draining` as soon as shutdown begins, so a probe steers traffic away.
    let drain_signal = shutdown.clone();
    tokio::spawn(async move {
        drain_signal.triggered().await;
        readiness.set(ReadyState::Draining);
    });

    let client = async_nats::connect(args.nats.as_str())
        .await
        .with_context(|| format!("could not connect to NATS at {}", args.nats))?;
    let context = async_nats::jetstream::new(client);

    // Read raw job bytes so the loop can size-gate them before decode.
    let jobs = JobsConfig::default();
    let stream = open_job_stream(&context, &loaded.region.id, &jobs)
        .await
        .context("could not open the region job stream")?;
    let consumer = job_consumer(&stream, &loaded.region.graph, &loaded.region.id, &jobs)
        .await
        .context("could not create the job consumer")?;
    let consumer = JetStreamConsumer::<RawBytes>::new(consumer);

    let engine = Arc::new(
        Engine::new(
            Arc::clone(&loaded.network),
            OsmEdgeMetadata::runtime(None),
            args.search_distance,
        )
        .with_max_candidates(Some(args.max_candidates).filter(|k| *k > 0))
        .with_window_layers(Some(args.window_layers).filter(|w| *w > 0)),
    );

    let publisher = ResultPublisher::new(
        JetStreamPublisher::<SolveResult<E>>::new(context.clone(), PUBLISH_ACK_TIMEOUT),
        PublishConfig::default(),
    );

    let cfg = PullConfig {
        slots: args.slots,
        fetch_wait: FETCH_WAIT,
        max_batch: args.slots,
        validate: ValidateConfig {
            max_decoded_bytes: args.max_decoded_bytes,
            min_remaining: ValidateConfig::DEFAULT_MIN_REMAINING,
        },
        grace: args.grace,
    };

    info!(
        region = %loaded.region.id,
        graph = %loaded.region.graph,
        slots = args.slots,
        "matcher ready; pulling jobs"
    );

    let pull = PullLoop::new(
        engine,
        consumer,
        publisher,
        loaded.region.clone(),
        loaded.cells.clone(),
        cfg,
        shutdown,
        Drain::new(),
    )
    .with_metrics(metrics);

    let stats = pull.run().await;
    info!(?stats, "matcher drained; exiting");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Vec<&'static str> {
        vec![
            "matcher",
            "--nats",
            "nats://localhost:4222",
            "--catalog",
            "/etc/routers/catalog.toml",
            "--region",
            "syd",
            "--shard-dir",
            "/var/lib/routers/shards",
        ]
    }

    #[test]
    fn parses_required_fields_and_defaults() {
        let args = Args::parse_from(base());

        assert_eq!(args.nats.as_str(), "nats://localhost:4222");
        assert_eq!(args.catalog, PathBuf::from("/etc/routers/catalog.toml"));
        assert_eq!(args.region, RegionId::new("syd").unwrap());
        assert_eq!(args.shard_dir, PathBuf::from("/var/lib/routers/shards"));

        assert_eq!(args.slots, default_slots());
        assert_eq!(args.grace, Duration::from_secs(20));
        assert_eq!(
            args.max_decoded_bytes,
            ValidateConfig::DEFAULT_MAX_DECODED_BYTES
        );
        assert!(args.ready_file.is_none());
        assert!(args.search_distance.is_none());
    }

    #[test]
    fn parses_the_full_optional_set() {
        let mut argv = base();
        argv.extend([
            "--slots",
            "8",
            "--ready-file",
            "/run/matcher.ready",
            "--grace",
            "45s",
            "--max-decoded-bytes",
            "1048576",
            "--search-distance",
            "35.5",
        ]);

        let args = Args::parse_from(argv);

        assert_eq!(args.slots, 8);
        assert_eq!(args.ready_file, Some(PathBuf::from("/run/matcher.ready")));
        assert_eq!(args.grace, Duration::from_secs(45));
        assert_eq!(args.max_decoded_bytes, 1 << 20);
        assert_eq!(args.search_distance, Some(35.5));
    }

    #[test]
    fn rejects_a_non_token_safe_region() {
        let mut argv = base();
        let region_index = argv.iter().position(|arg| *arg == "syd").unwrap();
        argv[region_index] = "not/a/token";

        assert!(Args::try_parse_from(argv).is_err());
    }

    #[test]
    fn requires_the_mandatory_arguments() {
        assert!(Args::try_parse_from(["matcher"]).is_err());
    }

    #[test]
    fn arg_definitions_are_valid() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }
}
