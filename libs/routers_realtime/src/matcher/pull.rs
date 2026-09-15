//! Matcher capacity-bound pull loop: claim only as much work as there is CPU
//! and payload budget to answer, solve it, and answer every job. (T27)
//!
//! A region's matchers share one work-queue consumer (spec §5), so the broker
//! hands each job to exactly one replica and redelivers an un-acked one to
//! another. The loop's whole job is to *not over-claim*: solving is CPU-bound
//! and runs on the blocking pool, so the number of solves in flight is capped
//! at [`PullConfig::slots`] (one per core) and the loop never fetches more than
//! the free slots. Claiming a job it cannot start would only park work on this
//! replica that another idle replica could have taken, and — because a
//! work-queue job is invisible to peers until it is acked or its `ack_wait`
//! lapses — needlessly delay the vehicle's answer.
//!
//! Every claimed job is answered, never silently dropped (spec §4, §8):
//!
//! * bytes that cannot be turned into an answerable job ([`check_bytes`] ⇒
//!   [`PoisonReason`](super::validate::PoisonReason)) are acknowledged and
//!   dropped — there is no identity to publish a result against;
//! * a job that must not be solved ([`check`] ⇒ [`Checked::Refuse`]) has its
//!   typed [`SolveOutcome`] published as the result, so the owner learns why;
//! * a solvable job is handed to [`Engine::solve_blocking`] and its outcome
//!   published.
//!
//! Publishing a result and acknowledging its job are sequenced by
//! [`ResultPublisher::publish_then_ack`] (T26): the job is never acked until the
//! broker holds the result, and a publish that fails leaves the job for
//! redelivery. That is what makes a matcher crash safe — an unanswered job is
//! redelivered within `ack_wait`/`max_deliver` (spec §10) — at the cost of a
//! duplicate solve, which the result's msg-id deduplicates.
//!
//! # Raw bytes before decode
//!
//! The size gate must run *before* the postcard decode ([`check_bytes`]), so a
//! hostile length prefix can never drive an unbounded allocation. But
//! [`Consumer`] decodes eagerly into its `T`, so the loop consumes an identity
//! [`RawBytes`] payload — a `Wire` whose "decode" is just "keep the bytes" —
//! and gates them itself. [`RawBytes`] is public because the orchestrator's
//! worker (T23) reuses the same trick to read raw observations.
//!
//! # Concurrency without threads-crossing
//!
//! The bus adapter futures are deliberately not `Send` (see
//! [`bus::adapter`](crate::bus::adapter)): the loop owns them on one task and
//! never moves them across threads. Concurrency therefore comes from a
//! [`FuturesUnordered`] of in-flight job handlers driven on that one task — each
//! handler offloads its actual solve to [`tokio::task::spawn_blocking`], so the
//! CPU work still fans out over the pool — rather than from spawning `Send`
//! tasks onto a runtime. This is the one place this module departs from the
//! T27 draft's "`JoinSet`" wording, which would force `Send` bounds the adapter
//! contract forbids.

use alloc::sync::Arc;
use core::time::Duration;
use std::collections::HashSet;

use futures::stream::{FuturesUnordered, StreamExt};
use routers_network::Network;
use routers_shard::Geohash;
use tracing::{debug, warn};
use web_time::UNIX_EPOCH;

use crate::bus::adapter::{AckHandle, Consumer, Delivery, Publisher};
use crate::bus::{self, Wire};
use crate::lifecycle::{Drain, InFlight, QuiesceOutcome, Shutdown};
use crate::matcher::engine::Engine;
use crate::matcher::publish::ResultPublisher;
use crate::matcher::validate::{Checked, ValidateConfig, check, check_bytes};
use crate::protocol::result::SolveResult;
use crate::region::Region;

/// An identity [`Wire`] payload: the message bytes, carried through the
/// eager-decoding [`Consumer`] untouched so the pull loop can size-gate them
/// *before* the real decode (see the module docs).
///
/// Public so the orchestrator worker (T23) can consume raw observations the
/// same way. `encode`/`decode` are the identity map — no framing is added, so a
/// `RawBytes` round-trips a producer's exact bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RawBytes(pub Vec<u8>);

impl Wire for RawBytes {
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(self.0.clone())
    }

    fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(RawBytes(bytes.to_vec()))
    }
}

/// How the pull loop bounds and paces the work it claims.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PullConfig {
    /// The most solves allowed in flight at once — one per core, since a solve
    /// is CPU-bound. Also the ceiling on how many jobs are ever claimed but
    /// unanswered on this replica.
    pub slots: usize,
    /// How long a fetch waits for a batch to fill before returning what it has
    /// (possibly nothing). Bounds how long the loop blocks with free capacity.
    pub fetch_wait: Duration,
    /// The most jobs to claim in a single fetch. Capped at the free slots each
    /// call regardless; defaults to [`slots`](Self::slots).
    pub max_batch: usize,
    /// The wire/semantic bounds applied to every job before it is solved.
    pub validate: ValidateConfig,
    /// How long [`run`](PullLoop::run) waits for in-flight solves to finish on
    /// shutdown before abandoning them to redelivery.
    pub grace: Duration,
}

impl Default for PullConfig {
    fn default() -> Self {
        let slots = std::thread::available_parallelism().map_or(1, |n| n.get());
        Self {
            slots,
            fetch_wait: Duration::from_secs(1),
            max_batch: slots,
            validate: ValidateConfig::default(),
            grace: Duration::from_secs(25),
        }
    }
}

/// What one [`run`](PullLoop::run) processed, for the caller's log line and the
/// binary's exit accounting. Every fetched delivery lands in exactly one of the
/// terminal counters (`solved` + `refused` + `poison` + `publish_failures`) once
/// its handler completes — a handler still in flight when the grace budget
/// lapses is counted in none of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PullStats {
    /// Deliveries pulled off the job consumer.
    pub fetched: u64,
    /// Jobs solved and their result published.
    pub solved: u64,
    /// Jobs refused with a typed outcome that was published.
    pub refused: u64,
    /// Deliveries whose bytes were unanswerable and were acked and dropped.
    pub poison: u64,
    /// Handlers whose result could not be published (the job was left for
    /// redelivery by [`ResultPublisher`]).
    pub publish_failures: u64,
    /// `true` when every in-flight solve finished within the grace budget on
    /// shutdown; `false` when the budget lapsed with work still running.
    pub drained: bool,
}

/// The matcher's capacity-bound pull loop over one region's job consumer.
///
/// Generic over the loaded [`Network`] `N` (so tests use
/// [`MockNetwork`](routers_network::mock::MockNetwork) and the binary its
/// sharded graph), the raw-bytes [`Consumer`] `C`, and the result [`Publisher`]
/// `P`. It is constructed after [`bootstrap`](super::bootstrap) has loaded and
/// verified the graph and readiness is already [`Ready`](crate::lifecycle::ReadyState::Ready):
/// [`run`](Self::run) never checks readiness, it only stops on `shutdown`.
pub struct PullLoop<N: Network, C, P> {
    engine: Arc<Engine<N>>,
    consumer: C,
    publisher: ResultPublisher<P>,
    region: Region,
    cells: HashSet<Geohash>,
    cfg: PullConfig,
    shutdown: Shutdown,
    drain: Drain,
}

impl<N: Network, C, P> PullLoop<N, C, P> {
    /// Assemble a pull loop from its already-built dependencies.
    ///
    /// `region` and `cells` are the two pieces of a loaded region the validator
    /// needs (`loaded.region` and `loaded.cells`); `engine` wraps the same
    /// loaded network. `shutdown` and `drain` are shared with the rest of the
    /// binary so a signal stops this loop and its in-flight solves are counted
    /// in the process-wide drain.
    // Eight collaborators, each distinct and none sensibly grouped: the loop is
    // the join point of bootstrap, the bus, and lifecycle, so an explicit
    // constructor reads clearer than a bespoke parameter struct.
    #[allow(clippy::too_many_arguments)]
    #[must_use]
    pub fn new(
        engine: Arc<Engine<N>>,
        consumer: C,
        publisher: ResultPublisher<P>,
        region: Region,
        cells: HashSet<Geohash>,
        cfg: PullConfig,
        shutdown: Shutdown,
        drain: Drain,
    ) -> Self {
        Self {
            engine,
            consumer,
            publisher,
            region,
            cells,
            cfg,
            shutdown,
            drain,
        }
    }
}

impl<N, C, P> PullLoop<N, C, P>
where
    N: Network + 'static,
    N::Runtime: 'static,
    N::Entry: serde::de::DeserializeOwned,
    C: Consumer<RawBytes>,
    P: Publisher<SolveResult<N::Entry>>,
{
    /// Pull, solve, and answer jobs until `shutdown` is triggered.
    ///
    /// Each turn computes the free slots and, while any exist, fetches up to
    /// `min(free, max_batch)` jobs and spawns a handler for each into an
    /// in-flight set; when full, it only drains completions. Fetching and
    /// completing race on one task, so a result is published the moment its
    /// solve returns rather than waiting on the next fetch. On shutdown it stops
    /// fetching and waits up to [`PullConfig::grace`] for the in-flight solves
    /// to finish, then returns the run's [`PullStats`].
    pub async fn run(self) -> PullStats {
        let Self {
            engine,
            mut consumer,
            publisher,
            region,
            cells,
            cfg,
            shutdown,
            drain,
        } = self;

        // Share the read-only validator inputs across every handler without a
        // per-job clone of the served-cell set.
        let region = Arc::new(region);
        let cells = Arc::new(cells);

        let mut stats = PullStats::default();
        let mut in_flight = FuturesUnordered::new();

        while !shutdown.is_triggered() {
            let free = cfg.slots.saturating_sub(in_flight.len());
            if free == 0 {
                // At capacity: only make room by finishing work (or stop).
                tokio::select! {
                    biased;
                    () = shutdown.triggered() => break,
                    Some(done) = in_flight.next() => record(done, &mut stats),
                }
                continue;
            }

            let want = free.min(cfg.max_batch.max(1));
            tokio::select! {
                biased;
                // Fetch first so a ready batch is claimed and never dropped by a
                // racing arm; a *pending* fetch has consumed nothing, so
                // dropping it when a completion wins is lossless.
                batch = consumer.fetch(want, cfg.fetch_wait) => match batch {
                    Ok(deliveries) => {
                        stats.fetched += deliveries.len() as u64;
                        for delivery in deliveries {
                            in_flight.push(handle_one(
                                delivery,
                                Arc::clone(&engine),
                                publisher.clone(),
                                Arc::clone(&region),
                                Arc::clone(&cells),
                                cfg.validate,
                                drain.begin(),
                            ));
                        }
                    }
                    Err(error) => warn!(%error, "job fetch failed; retrying"),
                },
                Some(done) = in_flight.next(), if !in_flight.is_empty() => {
                    record(done, &mut stats);
                }
                () = shutdown.triggered() => break,
            }
        }

        // Shutdown: stop fetching and let the in-flight solves finish. They live
        // on this task, so draining means polling them to completion while
        // `quiesce` watches the shared in-flight count and enforces the grace
        // budget.
        stats.drained = {
            let quiesce = drain.quiesce(cfg.grace);
            tokio::pin!(quiesce);
            loop {
                tokio::select! {
                    biased;
                    Some(done) = in_flight.next(), if !in_flight.is_empty() => {
                        record(done, &mut stats);
                    }
                    outcome = &mut quiesce => break matches!(outcome, QuiesceOutcome::Drained),
                }
            }
        };

        stats
    }
}

/// The terminal disposition of one delivery, folded into [`PullStats`].
enum Handled {
    /// Unanswerable bytes: acked and dropped.
    Poison,
    /// Refused with a typed outcome that was published.
    Refused,
    /// Solved and the result published.
    Solved,
    /// The result could not be published; the job was left for redelivery.
    PublishFailed,
}

/// Fold one completed handler's disposition into the running totals.
fn record(done: Handled, stats: &mut PullStats) {
    match done {
        Handled::Poison => stats.poison += 1,
        Handled::Refused => stats.refused += 1,
        Handled::Solved => stats.solved += 1,
        Handled::PublishFailed => stats.publish_failures += 1,
    }
}

/// Handle exactly one delivery end to end: size-gate, validate, solve or refuse,
/// publish, and acknowledge — holding `_guard` for its whole life so the drain
/// phase can wait on it.
async fn handle_one<N, H, P>(
    delivery: Delivery<RawBytes, H>,
    engine: Arc<Engine<N>>,
    publisher: ResultPublisher<P>,
    region: Arc<Region>,
    cells: Arc<HashSet<Geohash>>,
    validate: ValidateConfig,
    _guard: InFlight,
) -> Handled
where
    N: Network + 'static,
    N::Runtime: 'static,
    N::Entry: serde::de::DeserializeOwned,
    H: AckHandle,
    P: Publisher<SolveResult<N::Entry>>,
{
    let sent_at = delivery.sent_at;
    let handle = delivery.handle;
    let RawBytes(bytes) = delivery.item;

    // 1. Size-gate then decode. Unanswerable bytes are acked and dropped.
    let job = match check_bytes::<N::Entry>(&bytes, &validate) {
        Ok(job) => job,
        Err(reason) => {
            debug!(%reason, "dropping unanswerable job bytes");
            let _ = handle.ack().await;
            return Handled::Poison;
        }
    };

    // Record the delivery's queue wait as a span, mirroring the old binary.
    if let Some(sent) = sent_at {
        bus::span_between("queue_wait", sent, bus::wallclock());
    }

    let now_us = unix_micros();
    match check(&job, &region, &cells, now_us, &validate) {
        // 2. Refuse: publish the typed outcome so the owner sees an answer.
        Checked::Refuse(outcome) => {
            let result = SolveResult::new(&job, outcome, now_us);
            match publisher.publish_then_ack(&result, handle).await {
                Ok(_) => Handled::Refused,
                Err(error) => {
                    warn!(%error, job = %job.id, "failed to publish refusal");
                    Handled::PublishFailed
                }
            }
        }
        // 3. Solve on the blocking pool, then publish the outcome. The job is
        //    moved into the solve, so its identity (echoed into the result) is
        //    taken first; `id` and `identity` still come from the one job, so
        //    they cannot drift.
        Checked::Solve(_) => {
            let job_id = job.id;
            let identity = job.identity.clone();
            let started = bus::wallclock();
            let outcome = engine.solve_blocking(job).await;
            bus::span_between("solve_seconds", started, bus::wallclock());

            let result = SolveResult {
                job: job_id,
                identity,
                outcome,
                solved_at_us: unix_micros(),
            };
            match publisher.publish_then_ack(&result, handle).await {
                Ok(_) => Handled::Solved,
                Err(error) => {
                    warn!(%error, job = %job_id, "failed to publish solve result");
                    Handled::PublishFailed
                }
            }
        }
        // `check` never yields `Poison` — that is `check_bytes`' domain, handled
        // above — but the shared verdict type carries the variant.
        Checked::Poison(reason) => {
            debug!(%reason, "unexpected poison verdict from check; acking");
            let _ = handle.ack().await;
            Handled::Poison
        }
    }
}

/// The current wall clock as absolute unix microseconds, saturating rather than
/// wrapping if the clock is somehow before the epoch or beyond `i64`.
fn unix_micros() -> i64 {
    bus::wallclock()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_micros()).ok())
        .unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::sync::Arc;
    use core::time::Duration;

    use async_nats::HeaderMap;
    use geo::point;
    use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
    use routers_transition::{Continuation, Origin};

    use crate::bus::Wire;
    use crate::bus::adapter::{Publisher, Source};
    use crate::bus::memory::{MemoryBus, MemoryConsumer, MemoryPublisher};
    use crate::event::{self, VehicleId};
    use crate::lifecycle::{Drain, DrainReason, Shutdown};
    use crate::matcher::engine::Engine;
    use crate::matcher::publish::{PublishConfig, ResultPublisher};
    use crate::protocol::ids::{GraphVersion, Lane, ObservationId, RegionId, SCHEMA_VERSION};
    use crate::protocol::job::{JobIdentity, SolveJob};
    use crate::protocol::result::{SolveOutcome, SolveResult};
    use crate::region::{Region, Replicas};
    use crate::topology::jobs::{job_stream_subjects, job_subject};

    const GRAPH: &str = "v1";
    const REGION: &str = "region";
    /// The result-plane wildcard the tests inspect and drive shutdown from.
    const RESULTS: &str = "solve-result.v1.p.>";

    /// A staircase road (the shared `matched_diff` shape) the mock solves over.
    fn bent_road() -> MockNetwork {
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

    /// Six spaced observations tracing the bend.
    fn observations() -> Vec<Origin> {
        [
            point!(x: -118.151, y: 34.1503),
            point!(x: -118.155, y: 34.1503),
            point!(x: -118.165, y: 34.1503),
            point!(x: -118.170, y: 34.1490),
            point!(x: -118.172, y: 34.1403),
            point!(x: -118.179, y: 34.1403),
        ]
        .into_iter()
        .enumerate()
        .map(|(index, point)| Origin::new(point, 1_775_000_000_000_000 + index as i64 * 5_000_000))
        .collect()
    }

    fn engine() -> Arc<Engine<MockNetwork>> {
        Arc::new(Engine::new(Arc::new(bent_road()), (), None))
    }

    /// Every cell the trace touches, so a served-coverage region can be built.
    fn served_cells() -> HashSet<Geohash> {
        observations()
            .iter()
            .map(|origin| event::shard_of(origin.point))
            .collect()
    }

    fn region() -> Region {
        Region {
            id: RegionId::new(REGION).unwrap(),
            graph: GraphVersion::new(GRAPH).unwrap(),
            coverage: served_cells().into_iter().collect(),
            overlap: Vec::new(),
            lanes: 1,
            resource_class: "cpu-1".to_owned(),
            replicas: Replicas { min: 1, max: 1 },
            freshness_budget: Duration::from_millis(1000),
        }
    }

    fn identity(vehicle: u64) -> JobIdentity {
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(vehicle),
            observation: ObservationId {
                partition: 0,
                sequence: vehicle,
            },
            base: None,
            graph: GraphVersion::new(GRAPH).unwrap(),
            region: RegionId::new(REGION).unwrap(),
        }
    }

    /// A well-formed restart job for `vehicle`, deadline `deadline_us`.
    fn restart_job(vehicle: u64, deadline_us: i64) -> SolveJob<MockEntryId> {
        SolveJob::new(
            identity(vehicle),
            Lane::DEFAULT,
            deadline_us,
            Continuation::Restart {
                fresh: observations(),
            },
        )
    }

    /// Tight timings so the tests never sleep for real.
    fn config(slots: usize) -> PullConfig {
        PullConfig {
            slots,
            fetch_wait: Duration::from_millis(20),
            max_batch: slots,
            validate: ValidateConfig::default(),
            grace: Duration::from_secs(5),
        }
    }

    fn fast_publish() -> PublishConfig {
        PublishConfig {
            attempts: 5,
            backoff: Duration::from_millis(1),
            nak_delay: Duration::from_millis(20),
        }
    }

    fn job_filter() -> String {
        job_stream_subjects(&RegionId::new(REGION).unwrap())
    }

    /// Publish a job's exact wire bytes onto its region/lane subject, so the
    /// loop's raw consumer claims and size-gates them itself.
    async fn publish_job(bus: &MemoryBus, job: &SolveJob<MockEntryId>) {
        let bytes = job.encode().expect("encode job");
        let subject = job_subject(
            &GraphVersion::new(GRAPH).unwrap(),
            &RegionId::new(REGION).unwrap(),
            job.lane,
        );
        bus.publisher::<RawBytes>()
            .publish_bytes(&subject, &job.msg_id(), HeaderMap::new(), &bytes)
            .await
            .expect("publish job bytes");
    }

    type TestLoop =
        PullLoop<MockNetwork, MemoryConsumer<RawBytes>, MemoryPublisher<SolveResult<MockEntryId>>>;

    fn pull_loop(bus: &MemoryBus, cfg: PullConfig, shutdown: Shutdown) -> TestLoop {
        let consumer = bus.consumer::<RawBytes>(&job_filter());
        let publisher =
            ResultPublisher::new(bus.publisher::<SolveResult<MockEntryId>>(), fast_publish());
        PullLoop::new(
            engine(),
            consumer,
            publisher,
            region(),
            served_cells(),
            cfg,
            shutdown,
            Drain::new(),
        )
    }

    /// Poll `cond` on the shared bus until it holds, so a test can trigger
    /// shutdown on an observable side effect (an ack) rather than a sleep.
    async fn wait_until(cond: impl Fn() -> bool) {
        for _ in 0..1000 {
            if cond() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        panic!("condition was never met");
    }

    #[tokio::test]
    async fn solves_valid_jobs_and_acks_them() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        publish_job(&bus, &restart_job(1, i64::MAX)).await;
        publish_job(&bus, &restart_job(2, i64::MAX)).await;

        let mut results = bus.source::<SolveResult<MockEntryId>>(RESULTS);
        let pull = pull_loop(&bus, config(4), shutdown.clone());

        // Stop the loop once both results have landed, so the test is timing
        // free: it waits on the effect, not a duration.
        let driver = async {
            for _ in 0..2 {
                let delivery = results.next().await.expect("a result").expect("it decodes");
                assert!(
                    matches!(delivery.item.outcome, SolveOutcome::Solved { .. }),
                    "each valid restart solves, got {:?}",
                    delivery.item.outcome
                );
            }
            shutdown.trigger(DrainReason::Operator);
        };

        let (stats, ()) = tokio::join!(pull.run(), driver);

        assert_eq!(stats.fetched, 2);
        assert_eq!(stats.solved, 2);
        assert_eq!(stats.refused, 0);
        assert_eq!(stats.poison, 0);
        assert_eq!(stats.publish_failures, 0);
        assert!(stats.drained);
        assert_eq!(bus.published(RESULTS).len(), 2, "one result per job");
        assert_eq!(bus.acked_count(&job_filter()), 2, "both jobs acked");
    }

    #[tokio::test]
    async fn expired_job_publishes_deadline_expired_and_acks() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        // A deadline in the distant past: valid graph/region/coverage, but no
        // time left, so it is refused rather than solved.
        publish_job(&bus, &restart_job(7, 1)).await;

        let mut results = bus.source::<SolveResult<MockEntryId>>(RESULTS);
        let pull = pull_loop(&bus, config(2), shutdown.clone());

        let driver = async {
            let delivery = results.next().await.expect("a result").expect("it decodes");
            assert!(
                matches!(delivery.item.outcome, SolveOutcome::DeadlineExpired),
                "an expired job is refused as DeadlineExpired, got {:?}",
                delivery.item.outcome
            );
            shutdown.trigger(DrainReason::Operator);
        };

        let (stats, ()) = tokio::join!(pull.run(), driver);

        assert_eq!(stats.refused, 1);
        assert_eq!(stats.solved, 0);
        assert!(stats.drained);
        assert_eq!(
            bus.acked_count(&job_filter()),
            1,
            "the refused job is acked"
        );
    }

    #[tokio::test]
    async fn oversized_blob_is_acked_without_a_result() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        let cfg = PullConfig {
            validate: ValidateConfig {
                max_decoded_bytes: 16,
                ..ValidateConfig::default()
            },
            ..config(2)
        };

        // A blob past the decode bound: refused before decode, so there is no
        // identity to answer — it is acked and dropped.
        let subject = job_subject(
            &GraphVersion::new(GRAPH).unwrap(),
            &RegionId::new(REGION).unwrap(),
            Lane::DEFAULT,
        );
        bus.publisher::<RawBytes>()
            .publish_bytes(&subject, "oversized-1", HeaderMap::new(), &[0xAB_u8; 64])
            .await
            .expect("publish blob");

        let pull = pull_loop(&bus, cfg, shutdown.clone());
        let driver = async {
            wait_until(|| bus.acked_count(&job_filter()) == 1).await;
            shutdown.trigger(DrainReason::Operator);
        };

        let (stats, ()) = tokio::join!(pull.run(), driver);

        assert_eq!(stats.poison, 1);
        assert_eq!(stats.solved, 0);
        assert_eq!(stats.refused, 0);
        assert!(
            bus.published(RESULTS).is_empty(),
            "poison bytes publish no result"
        );
        assert_eq!(bus.acked_count(&job_filter()), 1, "the blob is acked");
    }

    #[tokio::test]
    async fn shutdown_ends_run_and_reports_drained() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        // No jobs: the loop idles in fetch until shutdown ends it cleanly.
        let pull = pull_loop(&bus, config(2), shutdown.clone());

        let driver = async {
            // Let the loop reach its first fetch, then stop it mid-run.
            tokio::time::sleep(Duration::from_millis(10)).await;
            shutdown.trigger(DrainReason::Signal);
        };

        let (stats, ()) = tokio::join!(pull.run(), driver);

        assert!(stats.drained, "an idle loop drains immediately");
        assert_eq!(stats.fetched, 0);
        assert_eq!(stats.solved, 0);
    }
}
