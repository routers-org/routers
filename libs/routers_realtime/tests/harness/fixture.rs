//! The deterministic, in-process fleet the load/failure scenarios drive. (T31)
//!
//! Everything the orchestrator, matcher and materialiser do is state and pure
//! logic over the [`bus::adapter`](routers_realtime::bus::adapter) and
//! [`CheckpointStore`](routers_realtime::store::checkpoint::CheckpointStore)
//! seams, so the whole pipeline runs against the in-memory fakes with no NATS or
//! Valkey in sight. This module assembles those fakes into a [`Fleet`]: a shared
//! [`MemoryBus`] and [`MemoryCheckpointStore`], an inline-TOML [`Catalog`], an
//! [`Admission`] controller, and spawners for the three long-lived services
//! (orchestrator partition worker, matcher, materialiser).
//!
//! # Why the pieces are wired the way the binaries wire them
//!
//! [`Fleet::spawn_orchestrator`] mirrors `bin/orchestrator.rs`: it runs
//! [`recover_partition`](routers_realtime::orchestrator::recovery::recover_partition)
//! first, then a [`PartitionWorker`] built over the memory adapters, so recovery
//! and the running loop are exercised exactly as production sequences them. The
//! matcher and materialiser reuse
//! [`materializer::consumer::run`](routers_realtime::materializer::consumer::run)
//! and the real [`Engine`], so a scenario tests the shipped code rather than a
//! stand-in.
//!
//! # Determinism under paused time
//!
//! Every scenario runs under `#[tokio::test(start_paused = true)]`. There are no
//! wall-clock sleeps: [`Fleet::settle`] advances the paused clock in small steps
//! until the observable bus state stops changing (a quiet window longer than any
//! timer gap a scenario arms), and [`advance_until`] advances until a predicate
//! holds. Both are bounded and panic with a diagnostic dump rather than hang, so
//! a regression fails loudly instead of wedging the suite.

#![allow(dead_code)]

use alloc::collections::BTreeSet;
use alloc::sync::Arc;
use core::time::Duration;

use async_nats::HeaderMap;
use chrono::{DateTime, Utc};
use geo::{Point, point};
use tokio::task::JoinHandle;

use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
use routers_network::{DirectionAwareEdgeId, Edge};
use routers_transition::matcher::Trip;

use routers_realtime::bus::Wire;
use routers_realtime::bus::adapter::{AckHandle, PublishOutcome, Publisher, Source};
use routers_realtime::bus::memory::{MemoryBus, MemoryPublisher, MemorySource};
use routers_realtime::event::{MatchedDiff, MatchedLayer, Payload, shard_of};
use routers_realtime::ingress;
use routers_realtime::lifecycle::{DrainReason, Shutdown};
use routers_realtime::matcher::engine::Engine;
use routers_realtime::matcher::pull::RawBytes;
use routers_realtime::materializer::consumer::{self as materializer, Stats as MaterializerStats};
use routers_realtime::materializer::memory::MemorySink;
use routers_realtime::orchestrator::admission::Admission;
use routers_realtime::orchestrator::commit::{CommitConfig, Committer};
use routers_realtime::orchestrator::dispatch::Dispatcher;
use routers_realtime::orchestrator::recovery::recover_partition;
use routers_realtime::orchestrator::worker::{PartitionWorker, WorkerConfig};
use routers_realtime::partition::partition_of;
use routers_realtime::protocol::ids::headers;
use routers_realtime::protocol::ids::{GraphVersion, RegionId, SCHEMA_VERSION};
use routers_realtime::protocol::job::{JobIdentity, SolveJob};
use routers_realtime::protocol::result::SolveResult;
use routers_realtime::region::catalog::Catalog;
use routers_realtime::store::checkpoint::{CommitPhase, PreparedCommit};
use routers_realtime::topology::{output_subject, raw_subject, result_subject};

// Re-export the handful of crate types the scenario modules name, so a scenario
// can `use super::fixture::*;` and reach them without a second import block.
pub use routers_realtime::event::VehicleId;
pub use routers_realtime::orchestrator::admission::AdmissionConfig;
pub use routers_realtime::orchestrator::worker::WorkerStats;
pub use routers_realtime::protocol::ids::{ObservationId, Revision, SegmentId};
pub use routers_realtime::protocol::output::{
    CommittedOutput, OutputKind, ResetReason, TerminalReason,
};
pub use routers_realtime::protocol::result::SolveOutcome;
pub use routers_realtime::store::checkpoint::{CheckpointStore, MemoryCheckpointStore, Op};

/// The network entry type the whole fleet solves against — the mock the crate's
/// other tests use, so the harness needs no real OSM graph.
pub type E = MockEntryId;

/// The fully-specialised worker the harness runs: memory bus, memory store, and
/// the four publisher/source types spelt out so every scenario shares one alias.
pub type Worker = PartitionWorker<
    E,
    MemoryCheckpointStore,
    MemoryPublisher<SolveJob<E>>,
    MemoryPublisher<CommittedOutput<E>>,
    MemorySource<RawBytes>,
    MemorySource<SolveResult<E>>,
>;

/// The first supplier stamp in a trace; later observations are spaced from it.
pub const TRACE_START_US: i64 = 1_775_000_000_000_000;
/// The spacing between consecutive observations (five seconds), well under the
/// default 120 s continuity gap so a normal trace never resets.
pub const OBS_SPACING_US: i64 = 5_000_000;

// ---------------------------------------------------------------------------
// The bent-road network and its served cell
// ---------------------------------------------------------------------------

/// A staircase road (west, south, west) — the same shape `tests/matched_diff.rs`
/// and the engine's own tests match against, so a real solve produces real
/// layers.
#[must_use]
pub fn bent_road() -> MockNetwork {
    MockNetworkBuilder::new()
        .node(1, point!(x: -118.15, y: 34.15))
        .node(2, point!(x: -118.16, y: 34.15))
        .node(3, point!(x: -118.17, y: 34.15))
        .node(4, point!(x: -118.17, y: 34.14))
        .node(5, point!(x: -118.18, y: 34.14))
        .edge(1, 2)
        .edge(2, 3)
        .edge(3, 4)
        .edge(4, 5)
        .build()
}

/// Six observations tracing the bend, spaced five seconds apart with realistic
/// supplier stamps. Index `i` is stamped [`TRACE_START_US`] + `i` × spacing.
#[must_use]
pub fn road_points() -> [Point; 6] {
    [
        point!(x: -118.151, y: 34.1503),
        point!(x: -118.155, y: 34.1503),
        point!(x: -118.165, y: 34.1503),
        point!(x: -118.170, y: 34.1490),
        point!(x: -118.172, y: 34.1403),
        point!(x: -118.179, y: 34.1403),
    ]
}

/// The supplier stamp of the `i`th observation in a trace.
#[must_use]
pub fn obs_ts(i: usize) -> i64 {
    TRACE_START_US + i as i64 * OBS_SPACING_US
}

/// The distinct precision-4 coverage cells the bent-road trace touches, computed
/// (never hard-coded) from [`shard_of`]. In practice the whole staircase falls in
/// one cell, but computing the set keeps the fixture correct even if a point
/// straddles a geohash boundary.
#[must_use]
pub fn road_cells() -> Vec<String> {
    cells_of(&road_points())
}

/// The distinct coverage cells a set of points occupies, in sorted order.
#[must_use]
pub fn cells_of(points: &[Point]) -> Vec<String> {
    points
        .iter()
        .map(|&p| shard_of(p).to_string())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// A shared engine over the bent road, ready to hand to a [`MatcherBehaviour`].
#[must_use]
pub fn engine() -> Arc<Engine<MockNetwork>> {
    Arc::new(Engine::new(Arc::new(bent_road()), (), None))
}

// ---------------------------------------------------------------------------
// Building committed outputs, results and layers for the scenarios
// ---------------------------------------------------------------------------

/// A single matched layer at `timestamp`, with placeholder geometry. Enough for
/// the materialiser to key and merge it; the geometry is never asserted on.
#[must_use]
pub fn layer_at(timestamp: i64) -> MatchedLayer<E> {
    MatchedLayer {
        timestamp,
        edge: Edge {
            source: MockEntryId(1),
            target: MockEntryId(2),
            weight: 1,
            id: DirectionAwareEdgeId::new(MockEntryId(3)),
        },
        position: Point::new(0.0, 0.0),
        path: Vec::new(),
    }
}

/// A scripted `Solved` outcome carrying one layer at `timestamp`, so a commit
/// materialises a real, addressable layer without a live network.
#[must_use]
pub fn solved_with_layer(timestamp: i64) -> SolveOutcome<E> {
    SolveOutcome::Solved {
        diff: MatchedDiff {
            revision: 0,
            downgraded: false,
            layers: vec![layer_at(timestamp)],
        },
        trip: Trip::new(),
        converged_through: None,
    }
}

/// A scripted `Solved` outcome with an empty diff — a commit with no layers, for
/// the flow tests that assert on counts rather than materialised geometry.
#[must_use]
pub fn empty_solved() -> SolveOutcome<E> {
    SolveOutcome::Solved {
        diff: MatchedDiff {
            revision: 0,
            downgraded: false,
            layers: Vec::new(),
        },
        trip: Trip::new(),
        converged_through: None,
    }
}

// ---------------------------------------------------------------------------
// The matcher behaviour
// ---------------------------------------------------------------------------

/// A scripted answer function: given the job, produce its outcome, or `None` to
/// stay silent (never answer — the deadline path's fuel).
pub type Answer = Box<dyn FnMut(&SolveJob<E>) -> Option<SolveOutcome<E>> + Send>;

/// How a spawned matcher answers the jobs it pulls.
pub enum MatcherBehaviour {
    /// Solve each job with the real [`Engine`] over the bent road.
    Engine(Arc<Engine<MockNetwork>>),
    /// Answer with a scripted function; `None` never answers.
    Scripted(Answer),
    /// Wait `by` (paused time) before answering with `then`.
    Delayed {
        /// How long to hold each answer back.
        by: Duration,
        /// The scripted answer applied after the delay.
        then: Answer,
    },
}

impl MatcherBehaviour {
    /// A matcher that solves every job with a fresh bent-road engine.
    #[must_use]
    pub fn engine() -> Self {
        MatcherBehaviour::Engine(engine())
    }

    /// A matcher that answers every job with one layer at the job head's stamp.
    #[must_use]
    pub fn scripted_layer() -> Self {
        MatcherBehaviour::Scripted(Box::new(|job| {
            job.head().map(|origin| solved_with_layer(origin.timestamp))
        }))
    }

    /// A matcher that never answers, so every job runs out its deadline.
    #[must_use]
    pub fn silent() -> Self {
        MatcherBehaviour::Scripted(Box::new(|_| None))
    }

    /// A matcher that waits `by` before answering each job with one layer at its
    /// head stamp — for the "the answer lands just before the deadline" race.
    #[must_use]
    pub fn delayed_layer(by: Duration) -> Self {
        MatcherBehaviour::Delayed {
            by,
            then: Box::new(|job| job.head().map(|origin| solved_with_layer(origin.timestamp))),
        }
    }
}

// ---------------------------------------------------------------------------
// Service handles
// ---------------------------------------------------------------------------

/// A running partition worker. [`stop`](Self::stop) drains it and returns its
/// [`WorkerStats`]; [`crash`](Self::crash) aborts the task *without* draining —
/// the crash-boundary primitive that leaves the durable store exactly as a hard
/// kill would.
pub struct OrchestratorHandle {
    shutdown: Shutdown,
    join: JoinHandle<anyhow::Result<WorkerStats>>,
}

impl OrchestratorHandle {
    /// Signal a graceful drain and wait for the worker's exit stats.
    pub async fn stop(self) -> WorkerStats {
        self.shutdown.trigger(DrainReason::Operator);
        self.join
            .await
            .expect("worker task joins")
            .expect("worker ran cleanly")
    }

    /// Abort the worker mid-flight without draining — a simulated crash. Any
    /// prepared commit, unacked raw, or un-advanced frontier is left as-is for a
    /// later [`Fleet::spawn_orchestrator`] to recover.
    pub fn crash(self) {
        self.join.abort();
    }
}

/// A running matcher. [`stop`](Self::stop) ends its pull loop.
pub struct MatcherHandle {
    shutdown: Shutdown,
    join: JoinHandle<()>,
}

impl MatcherHandle {
    /// Stop the matcher and wait for its task to finish.
    pub async fn stop(self) {
        self.shutdown.trigger(DrainReason::Operator);
        self.join.await.expect("matcher task joins");
    }
}

/// A running materialiser. [`stop`](Self::stop) ends its consume loop and returns
/// its [`MaterializerStats`].
pub struct MaterializerHandle {
    shutdown: Shutdown,
    join: JoinHandle<anyhow::Result<MaterializerStats>>,
}

impl MaterializerHandle {
    /// Stop the materialiser and return its run stats.
    pub async fn stop(self) -> MaterializerStats {
        self.shutdown.trigger(DrainReason::Operator);
        self.join
            .await
            .expect("materialiser task joins")
            .expect("materialiser ran cleanly")
    }
}

// ---------------------------------------------------------------------------
// The fleet
// ---------------------------------------------------------------------------

/// One in-process fleet: the shared bus and store, the region catalog, and the
/// admission controller. Cheap to clone the pieces out of (`bus`/`store` are
/// `Arc`-backed), so a scenario keeps its own handles for assertions.
pub struct Fleet {
    /// The shared in-memory bus every plane rides on.
    pub bus: MemoryBus,
    /// The shared in-memory checkpoint store.
    pub store: MemoryCheckpointStore,
    /// The region catalog resolved against.
    pub catalog: Arc<Catalog>,
    /// The admission controller shared by the partition workers.
    pub admission: Admission,
    /// The per-region freshness budget baked into the catalog (the deadline).
    budget_ms: u64,
}

impl Fleet {
    /// A fleet serving `regions`, each `(id, &[cell, …])`, with the default 30 s
    /// freshness budget and default admission ceilings.
    #[must_use]
    pub fn new(regions: &[(&str, &[&str])]) -> Self {
        Self::with_budget(regions, 30_000)
    }

    /// As [`new`](Self::new), but with an explicit per-region freshness budget so
    /// the deadline scenarios can make jobs expire quickly.
    #[must_use]
    pub fn with_budget(regions: &[(&str, &[&str])], budget_ms: u64) -> Self {
        let catalog = Arc::new(build_catalog(regions, budget_ms));
        let admission = Admission::new(
            AdmissionConfig::default(),
            catalog.regions.iter().map(|r| &r.id),
        );
        Self {
            bus: MemoryBus::new(),
            store: MemoryCheckpointStore::new(),
            catalog,
            admission,
            budget_ms,
        }
    }

    /// A one-region fleet serving the bent road, the common case: region `r1`
    /// covering exactly the cells the trace touches (computed, not hard-coded).
    #[must_use]
    pub fn bent_road() -> Self {
        Self::bent_road_with_budget(30_000)
    }

    /// The bent-road fleet with a caller-chosen freshness budget.
    #[must_use]
    pub fn bent_road_with_budget(budget_ms: u64) -> Self {
        let cells = road_cells();
        let cell_refs: Vec<&str> = cells.iter().map(String::as_str).collect();
        Self::with_budget(&[("r1", &cell_refs)], budget_ms)
    }

    /// Replace the admission controller with one built from `cfg`, so a scenario
    /// can tighten a ceiling (e.g. `region_jobs = 1`).
    #[must_use]
    pub fn with_admission(mut self, cfg: AdmissionConfig) -> Self {
        self.admission = Admission::new(cfg, self.catalog.regions.iter().map(|r| &r.id));
        self
    }

    /// The partition a vehicle id hashes to.
    #[must_use]
    pub fn partition_of(&self, vehicle: u64) -> u16 {
        partition_of(VehicleId(vehicle)) as u16
    }

    /// The first region's id — the region the one-region fleets pin to.
    #[must_use]
    pub fn region(&self) -> RegionId {
        self.catalog.regions[0].id.clone()
    }

    /// The first region's graph version.
    #[must_use]
    pub fn graph(&self) -> GraphVersion {
        self.catalog.regions[0].graph.clone()
    }

    // ----- ingress -----

    /// Publish one raw observation on its partition's journal with the ingress
    /// dedup key and a stamped schema header, returning its assigned
    /// [`ObservationId`] (the stream sequence is its revision downstream).
    pub async fn ingest(&self, vehicle: u64, ts_us: i64, point: Point) -> ObservationId {
        let partition = self.partition_of(vehicle);
        let payload = Payload {
            vehicle_id: VehicleId(vehicle),
            timestamp: DateTime::<Utc>::from_timestamp_micros(ts_us).expect("valid stamp"),
            point,
        };
        let bytes = payload.encode().expect("payload encodes");
        let mut hdrs = HeaderMap::new();
        headers::stamp_schema(&mut hdrs);
        let outcome = self
            .bus
            .publisher::<RawBytes>()
            .publish_bytes(
                &raw_subject(u64::from(partition)),
                &ingress::msg_id(&payload),
                hdrs,
                &bytes,
            )
            .await
            .expect("raw publish");
        let sequence = match outcome {
            PublishOutcome::Acked { sequence, .. } => sequence,
        };
        ObservationId {
            partition,
            sequence,
        }
    }

    /// Redeliver every delivered-but-unacked raw on `partition` (a transport
    /// redelivery / crash-recovery replay).
    pub fn redeliver_raw(&self, partition: u16) {
        self.bus
            .redeliver_unacked(&raw_subject(u64::from(partition)));
    }

    /// Redeliver every delivered-but-unacked result on `partition`.
    pub fn redeliver_results(&self, partition: u16) {
        self.bus
            .redeliver_unacked(&result_subject(u64::from(partition)));
    }

    // ----- building identities and results for the scenarios -----

    /// The identity of the job a fresh vehicle's first observation dispatches:
    /// no base, this fleet's region and graph. Its
    /// [`job_id`](JobIdentity::job_id) is what a pre-published result must echo
    /// to be accepted.
    #[must_use]
    pub fn fresh_identity(&self, vehicle: u64, observation: ObservationId) -> JobIdentity {
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(vehicle),
            observation,
            base: None,
            graph: self.graph(),
            region: self.region(),
        }
    }

    /// Publish a solve result for `identity` directly onto its partition's result
    /// plane — the "a straggler answers late" and "an early answer beats the
    /// dispatch" primitives.
    pub async fn publish_result(&self, identity: JobIdentity, outcome: SolveOutcome<E>) {
        let result = SolveResult {
            job: identity.job_id(),
            identity,
            outcome,
            solved_at_us: 0,
        };
        let partition = result.partition();
        self.bus
            .publisher::<SolveResult<E>>()
            .publish(
                &result_subject(u64::from(partition)),
                &result.msg_id(),
                HeaderMap::new(),
                &result,
            )
            .await
            .expect("result publish");
    }

    /// Publish a *second* copy of a result for `identity` under a distinct dedup
    /// key (`tag`), so the broker does not collapse it — a genuine duplicate
    /// delivery of an answer the worker has already decided, which it must
    /// reject or quarantine rather than re-commit.
    pub async fn publish_result_dup(
        &self,
        identity: JobIdentity,
        outcome: SolveOutcome<E>,
        tag: &str,
    ) {
        let result = SolveResult {
            job: identity.job_id(),
            identity,
            outcome,
            solved_at_us: 0,
        };
        let partition = result.partition();
        let msg_id = format!("{}-{tag}", result.msg_id());
        self.bus
            .publisher::<SolveResult<E>>()
            .publish(
                &result_subject(u64::from(partition)),
                &msg_id,
                HeaderMap::new(),
                &result,
            )
            .await
            .expect("duplicate result publish");
    }

    // ----- spawning services -----

    /// Recover `partition` and run a [`PartitionWorker`] over it, exactly as
    /// `bin/orchestrator.rs` sequences the two. Returns a handle that can drain
    /// or crash the worker.
    pub fn spawn_orchestrator(&self, partition: u16) -> OrchestratorHandle {
        let cfg = self.worker_config(partition);
        let dispatcher = Dispatcher::new(self.bus.publisher::<SolveJob<E>>(), cfg.dispatch);
        let committer = Committer::new(
            self.store.clone(),
            self.bus.publisher::<CommittedOutput<E>>(),
            cfg.commit.clone(),
        );
        let raw = self
            .bus
            .source::<RawBytes>(&raw_subject(u64::from(partition)));
        let results = self
            .bus
            .source::<SolveResult<E>>(&result_subject(u64::from(partition)));
        let shutdown = Shutdown::new();
        let store = self.store.clone();
        let catalog = self.catalog.clone();
        let admission = self.admission.clone();
        let worker_shutdown = shutdown.clone();

        let join = tokio::spawn(async move {
            // Resolve every surviving prepared commit before the loop reads.
            let report = recover_partition(&store, &committer, partition).await?;
            let worker = PartitionWorker::new(
                cfg,
                catalog,
                admission,
                store,
                dispatcher,
                committer,
                raw,
                results,
                &report,
                worker_shutdown,
            );
            worker.run().await
        });

        OrchestratorHandle { shutdown, join }
    }

    /// Spawn a matcher consuming the solve-jobs plane and answering per
    /// `behaviour`.
    pub fn spawn_matcher(&self, behaviour: MatcherBehaviour) -> MatcherHandle {
        let bus = self.bus.clone();
        let shutdown = Shutdown::new();
        let matcher_shutdown = shutdown.clone();
        let join = tokio::spawn(run_matcher(bus, behaviour, matcher_shutdown));
        MatcherHandle { shutdown, join }
    }

    /// Spawn a matcher that answers every job with one layer, but holds the
    /// answers for `slow_vehicle` back by `delay` — the out-of-order completion
    /// primitive, where one vehicle's answer lags another's.
    pub fn spawn_matcher_with_slow(&self, slow_vehicle: u64, delay: Duration) -> MatcherHandle {
        let bus = self.bus.clone();
        let shutdown = Shutdown::new();
        let matcher_shutdown = shutdown.clone();
        let join = tokio::spawn(run_matcher_slow(bus, slow_vehicle, delay, matcher_shutdown));
        MatcherHandle { shutdown, join }
    }

    /// Spawn a materialiser over the committed-output plane, returning its handle
    /// and a handle to the sink it writes so a scenario can read the view back.
    pub fn spawn_materializer(&self) -> (MaterializerHandle, MemorySink<E>) {
        let sink = MemorySink::<E>::new();
        let source = self
            .bus
            .source::<CommittedOutput<E>>("events.matched.v1.p.>");
        let shutdown = Shutdown::new();
        let run_shutdown = shutdown.clone();
        let run_sink = sink.clone();
        let join = tokio::spawn(async move {
            materializer::run::<E, _, _>(source, run_sink, run_shutdown).await
        });
        (MaterializerHandle { shutdown, join }, sink)
    }

    // ----- staging crash-boundary store state -----

    /// Directly install `bytes` as `vehicle`'s committed checkpoint at
    /// `revision`/`segment`, by staging a self-contained prepared commit and
    /// finishing it on a *throwaway* bus — so the seed's output never lands on
    /// the fleet's own bus. The only way to seed arbitrary (including corrupt)
    /// checkpoint bytes, mirroring the recovery unit tests.
    pub async fn seed_checkpoint_bytes(
        &self,
        vehicle: u64,
        partition: u16,
        revision: u64,
        expected_base: Option<Revision>,
        bytes: Vec<u8>,
    ) {
        let seed_bus = MemoryBus::new();
        let prepared = terminal_prepared(vehicle, partition, revision, expected_base, bytes);
        self.store
            .prepare(VehicleId(vehicle), partition, prepared.clone())
            .await
            .expect("seed prepare");
        Committer::new(
            self.store.clone(),
            seed_bus.publisher::<CommittedOutput<E>>(),
            zero_backoff_commit_config(),
        )
        .finish_prepared(VehicleId(vehicle), partition, prepared)
        .await
        .expect("seed installs the checkpoint bytes");
    }

    /// Stage a prepared commit whose output publish *fails*, leaving an
    /// unpublished prepared record on the store — the durable residue of a crash
    /// between prepare and publish. Recovery must publish then promote it.
    ///
    /// Returns the observation the record commits and the output's dedup id, so
    /// a scenario can assert exactly one output lands.
    pub async fn stage_unpublished(
        &self,
        vehicle: u64,
        partition: u16,
        revision: u64,
    ) -> ObservationId {
        use routers_realtime::bus::adapter::PublishError;
        let bytes = valid_checkpoint_bytes(vehicle, partition, revision);
        let prepared = terminal_prepared(vehicle, partition, revision, None, bytes);
        self.store
            .prepare(VehicleId(vehicle), partition, prepared.clone())
            .await
            .expect("stage prepare");
        // Finish on the fleet's own bus but with the next publish armed to fail,
        // so nothing lands and the prepared record survives unpublished.
        self.bus
            .fail_next_publish(PublishError::Failed(anyhow::anyhow!("bus down")));
        let err = Committer::new(
            self.store.clone(),
            self.bus.publisher::<CommittedOutput<E>>(),
            CommitConfig {
                publish_attempts: 1,
                backoff: Duration::ZERO,
            },
        )
        .finish_prepared(VehicleId(vehicle), partition, prepared)
        .await;
        assert!(err.is_err(), "the armed publish failure fails the finish");
        ObservationId {
            partition,
            sequence: revision,
        }
    }

    /// Stage a prepared commit that was *published but never promoted* — the
    /// residue of a crash between publish and promote. The output is already
    /// durable on the fleet's bus and the prepared record is marked
    /// [`CommitPhase::Published`]; recovery must republish (deduplicated) and
    /// promote it.
    pub async fn stage_published_unpromoted(
        &self,
        vehicle: u64,
        partition: u16,
        revision: u64,
    ) -> ObservationId {
        use routers_realtime::store::checkpoint::Op;
        let bytes = valid_checkpoint_bytes(vehicle, partition, revision);
        let prepared = terminal_prepared(vehicle, partition, revision, None, bytes);
        self.store
            .prepare(VehicleId(vehicle), partition, prepared.clone())
            .await
            .expect("stage prepare");
        // Publish and mark, but fail the promote: the output is durable and the
        // record is Published, only promotion is missing.
        self.store.fail_next(Op::Promote);
        let err = Committer::new(
            self.store.clone(),
            self.bus.publisher::<CommittedOutput<E>>(),
            zero_backoff_commit_config(),
        )
        .finish_prepared(VehicleId(vehicle), partition, prepared)
        .await;
        assert!(err.is_err(), "the armed promote failure fails the finish");
        ObservationId {
            partition,
            sequence: revision,
        }
    }

    // ----- driving time -----

    /// Advance the paused clock until the observable bus state (messages
    /// published, acknowledgements landed) has been unchanged for a quiet window
    /// wider than any timer gap the scenarios arm — i.e. the pipeline has
    /// quiesced. Bounded; panics with a diagnostic dump rather than hang.
    pub async fn settle(&self) {
        const STEP: Duration = Duration::from_millis(5);
        // 400 ms of simulated stillness: longer than a 250 ms tick, a 100 ms
        // deadline budget, or any inter-service delay a scenario injects, so the
        // detector never calls a pre-timer lull "quiescent".
        const QUIET_STEPS: usize = 80;
        const MAX_STEPS: usize = 6_000; // 30 s of simulated time, a hard ceiling.

        let mut last_change = 0usize;
        let mut prev = self.fingerprint();
        for step in 1..=MAX_STEPS {
            tokio::time::sleep(STEP).await;
            let now = self.fingerprint();
            if now != prev {
                prev = now;
                last_change = step;
            }
            if step - last_change >= QUIET_STEPS {
                return;
            }
        }
        panic!(
            "settle: pipeline never quiesced after {MAX_STEPS} steps; \
             last fingerprint {:?}",
            self.fingerprint()
        );
    }

    /// A cheap, order-free summary of everything observable on the bus: how many
    /// messages have been published and how many acknowledgements have landed,
    /// across every subject. Stable exactly when nothing is flowing.
    fn fingerprint(&self) -> (usize, usize) {
        (self.bus.published(">").len(), self.bus.acked_count(">"))
    }

    /// The worker configuration the harness runs: brisk tick and blocked-retry so
    /// timers resolve quickly under paused time, and a zero-backoff commit policy
    /// so a retried publish does not itself add a delay.
    fn worker_config(&self, partition: u16) -> WorkerConfig {
        WorkerConfig {
            tick: Duration::from_millis(20),
            blocked_retry: Duration::from_millis(20),
            commit: zero_backoff_commit_config(),
            ..WorkerConfig::new(partition)
        }
    }
}

// ---------------------------------------------------------------------------
// Free helpers
// ---------------------------------------------------------------------------

/// Advance the paused clock in 1 ms steps until `cond` holds, checking before the
/// first sleep. Bounded; panics rather than hang. The small step keeps the
/// caller ahead of the worker's 20 ms housekeeping tick, so a scenario can catch
/// a transient state (e.g. a blocked, mid-commit vehicle) before it is retried.
pub async fn advance_until(mut cond: impl FnMut() -> bool) {
    for _ in 0..10_000 {
        if cond() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("advance_until: condition never held");
}

/// The matcher's whole runtime: pull each job, answer per `behaviour`, ack the
/// job. Mirrors the scripted matcher in the worker's own unit tests, extended
/// with the engine and delayed behaviours.
async fn run_matcher(bus: MemoryBus, mut behaviour: MatcherBehaviour, shutdown: Shutdown) {
    let mut jobs = bus.source::<SolveJob<E>>("solve.v1.g.>");
    let publisher = bus.publisher::<SolveResult<E>>();
    loop {
        tokio::select! {
            biased;
            () = shutdown.triggered() => break,
            maybe = jobs.next() => match maybe {
                Some(Ok(delivery)) => {
                    let job = delivery.item;
                    let outcome = match &mut behaviour {
                        MatcherBehaviour::Engine(engine) => Some(engine.solve(&job)),
                        MatcherBehaviour::Scripted(answer) => answer(&job),
                        MatcherBehaviour::Delayed { by, then } => {
                            tokio::time::sleep(*by).await;
                            then(&job)
                        }
                    };
                    if let Some(outcome) = outcome {
                        let result = SolveResult::new(&job, outcome, 0);
                        let subject = result_subject(u64::from(result.partition()));
                        let _ = publisher
                            .publish(&subject, &result.msg_id(), HeaderMap::new(), &result)
                            .await;
                    }
                    let _ = delivery.handle.ack().await;
                }
                _ => break,
            },
        }
    }
}

/// A matcher that answers every job with one layer, delaying `slow_vehicle`'s
/// answers by `delay` — so a lower-sequence vehicle can complete *after* a
/// higher-sequence one, exercising contiguous frontier advance.
async fn run_matcher_slow(bus: MemoryBus, slow_vehicle: u64, delay: Duration, shutdown: Shutdown) {
    let mut jobs = bus.source::<SolveJob<E>>("solve.v1.g.>");
    let publisher = bus.publisher::<SolveResult<E>>();
    loop {
        tokio::select! {
            biased;
            () = shutdown.triggered() => break,
            maybe = jobs.next() => match maybe {
                Some(Ok(delivery)) => {
                    let job = delivery.item;
                    if job.identity.vehicle_id == VehicleId(slow_vehicle) {
                        tokio::time::sleep(delay).await;
                    }
                    if let Some(origin) = job.head() {
                        let outcome = solved_with_layer(origin.timestamp);
                        let result = SolveResult::new(&job, outcome, 0);
                        let subject = result_subject(u64::from(result.partition()));
                        let _ = publisher
                            .publish(&subject, &result.msg_id(), HeaderMap::new(), &result)
                            .await;
                    }
                    let _ = delivery.handle.ack().await;
                }
                _ => break,
            },
        }
    }
}

/// The zero-backoff commit policy the harness uses everywhere, so a retried
/// publish adds no wall delay under paused time.
fn zero_backoff_commit_config() -> CommitConfig {
    CommitConfig {
        publish_attempts: 4,
        backoff: Duration::ZERO,
    }
}

/// Build a self-contained terminal prepared commit whose promotion installs
/// `next_checkpoint` at `revision`. The output is a `Terminal` so it needs no
/// network to be valid; its bytes are the exact publish payload recovery replays.
fn terminal_prepared(
    vehicle: u64,
    partition: u16,
    revision: u64,
    expected_base: Option<Revision>,
    next_checkpoint: Vec<u8>,
) -> PreparedCommit {
    let observation = ObservationId {
        partition,
        sequence: revision,
    };
    let output = CommittedOutput::<E>::new(
        routers_realtime::protocol::ids::JobId(u128::from(revision)),
        VehicleId(vehicle),
        observation,
        Revision(revision),
        SegmentId(revision),
        OutputKind::Terminal {
            reason: TerminalReason::Unanchored,
            closes_segment: false,
        },
    );
    let entries: Vec<(String, String, Vec<u8>)> = vec![(
        output_subject(u64::from(partition)),
        output.msg_id(),
        output.encode().expect("output encodes"),
    )];
    PreparedCommit {
        output: output.id,
        output_subject: output_subject(u64::from(partition)),
        output_bytes: postcard::to_allocvec(&entries).expect("entries encode"),
        next_checkpoint,
        next_revision: Revision(revision),
        next_segment: SegmentId(revision),
        expected_base,
        phase: CommitPhase::Prepared,
        raw: observation,
    }
}

/// A minimal, decodable [`VehicleCheckpoint`] byte blob at `revision`, so a
/// staged commit promotes to a checkpoint the store can hand back cleanly.
fn valid_checkpoint_bytes(vehicle: u64, partition: u16, revision: u64) -> Vec<u8> {
    use routers_realtime::store::checkpoint::VehicleCheckpoint;
    let checkpoint = VehicleCheckpoint::<E> {
        trip: Trip::new(),
        last_input: ObservationId {
            partition,
            sequence: revision,
        },
        revision: Revision(revision),
        segment: SegmentId(revision),
        finalized_through: None,
        graph: GraphVersion::new("g-r1").expect("graph token"),
        schema: SCHEMA_VERSION,
        region: RegionId::new("r1").expect("region token"),
    };
    let _ = vehicle;
    checkpoint.encode().expect("checkpoint encodes")
}

/// Assemble an inline-TOML [`Catalog`] for `regions`, each `(id, cells)`, with a
/// generated graph version and the given freshness budget.
fn build_catalog(regions: &[(&str, &[&str])], budget_ms: u64) -> Catalog {
    let mut toml = String::from("version = 1\nrouting_version = 1\n");
    for (id, cells) in regions {
        let coverage = cells
            .iter()
            .map(|cell| format!("\"{cell}\""))
            .collect::<Vec<_>>()
            .join(", ");
        toml.push_str(&format!(
            "\n[[regions]]\n\
             id = \"{id}\"\n\
             graph = \"g-{id}\"\n\
             coverage = [{coverage}]\n\
             overlap = []\n\
             lanes = 1\n\
             resource_class = \"c\"\n\
             replicas = {{ min = 1, max = 1 }}\n\
             freshness_budget_ms = {budget_ms}\n",
        ));
    }
    Catalog::parse(&toml).expect("catalog parses")
}

/// A second vehicle id that hashes to the same partition as `vehicle` — for the
/// two-vehicles-share-a-worker scenarios.
#[must_use]
pub fn same_partition_as(vehicle: u64) -> u64 {
    let want = partition_of(VehicleId(vehicle)) as u16;
    (vehicle + 1..)
        .find(|&v| partition_of(VehicleId(v)) as u16 == want)
        .expect("another vehicle in the partition")
}

/// Decode every committed output currently on `partition`'s output plane, in
/// stream order — the assertion lens for what the pipeline emitted.
#[must_use]
pub fn published_outputs(bus: &MemoryBus, partition: u16) -> Vec<CommittedOutput<E>> {
    bus.published(&output_subject(u64::from(partition)))
        .into_iter()
        .map(|(_, _, bytes)| CommittedOutput::<E>::decode(&bytes).expect("output decodes"))
        .collect()
}
