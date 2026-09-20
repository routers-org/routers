//! The regional matcher worker binary.
//!
//! One matcher serves exactly one region: it loads that region's pinned graph
//! before pulling any job, then bounds end-to-end handlers separately from the
//! CPU solve stage and publishes each result before acknowledging the job so a
//! crash never loses work. This binary is only the wiring around the reusable
//! pieces in [`routers_realtime::matcher`].

// A binary crate has no `extern crate alloc`, so `std::sync::Arc` is the only spelling.
#![allow(clippy::std_instead_of_alloc)]

use core::net::SocketAddr;
use core::num::NonZeroUsize;
use core::time::Duration;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context as _;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::get;
use clap::Parser;
use tokio::net::TcpListener;
use tracing::info;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::Metadata;

use routers_realtime::bus::jetstream::{JetStreamConsumer, JetStreamPublisher};
use routers_realtime::lifecycle::{
    Drain, DrainReason, Readiness, ReadinessWatcher, ReadyState, Shutdown,
};
use routers_realtime::matcher::bootstrap::{BootstrapConfig, bootstrap};
use routers_realtime::matcher::engine::Engine;
use routers_realtime::matcher::publish::{PublishConfig, ResultPublisher};
use routers_realtime::matcher::pull::{PullConfig, PullLoop, RawBytes};
use routers_realtime::matcher::validate::ValidateConfig;
use routers_realtime::metrics::Metrics;
use routers_realtime::protocol::ids::{IdError, RegionId};
use routers_realtime::protocol::result::SolveResult;
use routers_realtime::secret::SecretUrl;
use routers_realtime::topology::jobs::{JobsConfig, job_consumer, open_job_stream};

/// The network entry type this region's graph is keyed by.
type E = OsmEntryId;

/// How long a result publish waits for the broker's ack before being retried idempotently.
const PUBLISH_ACK_TIMEOUT: Duration = Duration::from_secs(5);

/// How long one fetch waits for a batch to fill; short, so a lone job is not held back.
const FETCH_WAIT: Duration = Duration::from_millis(100);

/// Default address for Kubernetes liveness and readiness probes.
const DEFAULT_HEALTH_ADDR: &str = "0.0.0.0:9091";

#[derive(Clone)]
struct HealthState {
    readiness: ReadinessWatcher,
}

async fn live() -> StatusCode {
    StatusCode::OK
}

async fn ready(State(state): State<HealthState>) -> (StatusCode, &'static str) {
    let readiness = state.readiness.current();
    let status = if readiness == ReadyState::Ready {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (status, readiness.as_label())
}

fn health_router(readiness: ReadinessWatcher) -> Router {
    Router::new()
        .route("/live", get(live))
        .route("/ready", get(ready))
        .with_state(HealthState { readiness })
}

/// The default end-to-end handler capacity.
fn default_max_in_flight() -> NonZeroUsize {
    PullConfig::default().max_in_flight
}

/// The default CPU solve capacity.
fn default_solve_slots() -> NonZeroUsize {
    PullConfig::default().solve_slots
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
    nats: SecretUrl,

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

    /// The most jobs claimed but not yet answered, including publication and
    /// acknowledgement I/O.
    #[arg(long, default_value_t = default_max_in_flight())]
    max_in_flight: NonZeroUsize,

    /// The most CPU-bound solves to run concurrently. Defaults to the
    /// available parallelism.
    #[arg(long, default_value_t = default_solve_slots())]
    solve_slots: NonZeroUsize,

    /// Address for the Kubernetes liveness and readiness HTTP endpoints.
    #[arg(long, default_value = DEFAULT_HEALTH_ADDR)]
    health_addr: SocketAddr,

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

    let (readiness, watcher) = Readiness::new();
    let health_listener = TcpListener::bind(args.health_addr)
        .await
        .with_context(|| format!("could not bind health endpoint at {}", args.health_addr))?;
    let health_shutdown = shutdown.clone();
    let health_task = tokio::spawn(async move {
        let graceful = health_shutdown.clone();
        let result = axum::serve(health_listener, health_router(watcher))
            .with_graceful_shutdown(async move { graceful.triggered().await })
            .await;
        if result.is_err() {
            health_shutdown.trigger(DrainReason::Fatal);
        }
        result
    });

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
    let drain_readiness = readiness.clone();
    tokio::spawn(async move {
        drain_signal.triggered().await;
        drain_readiness.set(ReadyState::Draining);
    });

    let client = async_nats::connect(args.nats.connection_url().as_str())
        .await
        .context("could not connect to NATS")?;
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
        max_in_flight: args.max_in_flight,
        solve_slots: args.solve_slots,
        fetch_wait: FETCH_WAIT,
        max_batch: args.max_in_flight.get(),
        validate: ValidateConfig {
            max_decoded_bytes: args.max_decoded_bytes,
            min_remaining: ValidateConfig::DEFAULT_MIN_REMAINING,
        },
        grace: args.grace,
    };

    readiness.set(ReadyState::Ready);
    info!(
        region = %loaded.region.id,
        graph = %loaded.region.graph,
        max_in_flight = args.max_in_flight.get(),
        solve_slots = args.solve_slots.get(),
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

    health_task
        .await
        .context("health server task failed")?
        .context("health server failed")?;

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

        assert_eq!(args.nats.connection_url().as_str(), "nats://localhost:4222");
        assert_eq!(args.catalog, PathBuf::from("/etc/routers/catalog.toml"));
        assert_eq!(args.region, RegionId::new("syd").unwrap());
        assert_eq!(args.shard_dir, PathBuf::from("/var/lib/routers/shards"));

        assert_eq!(args.max_in_flight, default_max_in_flight());
        assert_eq!(args.solve_slots, default_solve_slots());
        assert_eq!(args.grace, Duration::from_secs(20));
        assert_eq!(
            args.max_decoded_bytes,
            ValidateConfig::DEFAULT_MAX_DECODED_BYTES
        );
        assert_eq!(args.health_addr, DEFAULT_HEALTH_ADDR.parse().unwrap());
        assert!(args.search_distance.is_none());
    }

    #[test]
    fn parses_the_full_optional_set() {
        let mut argv = base();
        argv.extend([
            "--max-in-flight",
            "48",
            "--solve-slots",
            "8",
            "--health-addr",
            "127.0.0.1:9191",
            "--grace",
            "45s",
            "--max-decoded-bytes",
            "1048576",
            "--search-distance",
            "35.5",
        ]);

        let args = Args::parse_from(argv);

        assert_eq!(args.max_in_flight, NonZeroUsize::new(48).unwrap());
        assert_eq!(args.solve_slots, NonZeroUsize::new(8).unwrap());
        assert_eq!(args.health_addr, "127.0.0.1:9191".parse().unwrap());
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
    fn rejects_zero_concurrency_limits() {
        for flag in ["--max-in-flight", "--solve-slots"] {
            let mut argv = base();
            argv.extend([flag, "0"]);
            assert!(Args::try_parse_from(argv).is_err(), "{flag} accepted zero");
        }
    }

    #[test]
    fn arg_definitions_are_valid() {
        use clap::CommandFactory;
        Args::command().debug_assert();
    }

    #[tokio::test]
    async fn readiness_endpoint_tracks_the_state_machine() {
        let (setter, watcher) = Readiness::new();
        let state = State(HealthState { readiness: watcher });

        assert_eq!(
            ready(state.clone()).await,
            (StatusCode::SERVICE_UNAVAILABLE, "starting")
        );

        setter.set(ReadyState::Ready);
        assert_eq!(ready(state).await, (StatusCode::OK, "ready"));
    }
}
