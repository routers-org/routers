//! Matcher capacity-bound pull loop: claim only as much work as there is
//! end-to-end handler capacity to answer, separately bound CPU solves, and
//! answer every job.
//!
//! A region's matchers share one work-queue consumer; the loop never fetches
//! more than its free [`PullConfig::max_in_flight`] so it does not park work a
//! peer could take. At sustained load it refills in half-capacity batches,
//! amortising broker pull requests while the remaining handlers stay busy. A
//! separate [`PullConfig::solve_slots`] limit admits work to
//! the blocking CPU stage without serialising validation, publication, or
//! acknowledgement. Every claimed job is answered, never silently dropped,
//! and [`ResultPublisher::publish_then_ack`] never acks a job until the broker
//! holds the result, so an unanswered job is redelivered (its msg-id dedups the
//! retry).

use alloc::sync::Arc;
use core::num::NonZeroUsize;
use core::sync::atomic::{AtomicUsize, Ordering};
use core::time::Duration;
use std::collections::HashSet;

use futures::stream::{FuturesUnordered, StreamExt};
use routers_network::Network;
use routers_shard::Geohash;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tracing::{debug, warn};
use web_time::UNIX_EPOCH;

use crate::bus::adapter::{AckHandle, Consumer, Delivery, Publisher};
use crate::bus::{self, Wire};
use crate::lifecycle::{Drain, InFlight, QuiesceOutcome, Shutdown};
use crate::matcher::engine::Engine;
use crate::matcher::publish::ResultPublisher;
use crate::matcher::validate::{Checked, ValidateConfig, check, check_bytes};
use crate::metrics::Metrics;
use crate::protocol::result::SolveResult;
use crate::region::Region;

/// An identity [`Wire`] payload: the message bytes carried through the
/// eager-decoding [`Consumer`] untouched so the pull loop can size-gate them
/// before the real decode. `encode`/`decode` are the identity map.
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
    /// The most claimed-but-unanswered jobs on this replica, including jobs
    /// validating, waiting for CPU, solving, publishing, or acknowledging.
    pub max_in_flight: NonZeroUsize,
    /// The most CPU-bound solves run concurrently. This limit is held only
    /// while [`Engine::solve_blocking`] is executing.
    pub solve_slots: NonZeroUsize,
    /// How long a fetch waits for a batch to fill before returning what it has.
    pub fetch_wait: Duration,
    /// The most jobs to claim in a single fetch; capped at the free handler
    /// capacity each call. Defaults to [`max_in_flight`](Self::max_in_flight).
    pub max_batch: usize,
    /// The wire/semantic bounds applied to every job before it is solved.
    pub validate: ValidateConfig,
    /// How long [`run`](PullLoop::run) waits for in-flight solves on shutdown
    /// before abandoning them to redelivery.
    pub grace: Duration,
}

impl Default for PullConfig {
    fn default() -> Self {
        let solve_slots = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
        Self {
            max_in_flight: NonZeroUsize::new(32).expect("32 is non-zero"),
            solve_slots,
            fetch_wait: Duration::from_secs(1),
            max_batch: 32,
            validate: ValidateConfig::default(),
            grace: Duration::from_secs(25),
        }
    }
}

/// What one [`run`](PullLoop::run) processed. A completed handler lands in
/// exactly one terminal counter; one still in flight when the grace budget
/// lapses is counted in none.
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
    /// Handlers whose result could not be published.
    pub publish_failures: u64,
    /// `true` when every in-flight solve finished within the grace budget.
    pub drained: bool,
}

/// The matcher's capacity-bound pull loop over one region's job consumer.
///
/// Generic over the loaded [`Network`] `N`, the raw-bytes [`Consumer`] `C`, and
/// the result [`Publisher`] `P`. Constructed after [`bootstrap`](super::bootstrap)
/// reached [`Ready`](crate::lifecycle::ReadyState::Ready); [`run`](Self::run)
/// never checks readiness, it only stops on `shutdown`.
pub struct PullLoop<N: Network, C, P> {
    engine: Arc<Engine<N>>,
    consumer: C,
    publisher: ResultPublisher<P>,
    region: Region,
    cells: HashSet<Geohash>,
    cfg: PullConfig,
    shutdown: Shutdown,
    drain: Drain,
    metrics: Metrics,
}

impl<N: Network, C, P> PullLoop<N, C, P> {
    /// Assemble a pull loop from its already-built dependencies.
    ///
    /// `region` and `cells` are the validator's two pieces of a loaded region;
    /// `engine` wraps the same loaded network. `shutdown` and `drain` are shared
    /// with the rest of the binary.
    // An explicit constructor reads clearer than a bespoke parameter struct.
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
            metrics: Metrics::noop(),
        }
    }

    /// Attach a metrics handle so the loop records solve, queue, and publish
    /// latencies and a `graph_ready` gauge. Without this it uses [`Metrics::noop`].
    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
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
    /// It fetches up to `min(free, max_batch)` jobs into an in-flight set. At
    /// sustained load it drains until half a batch is free before fetching,
    /// avoiding a broker round trip for every single completed handler. At most
    /// [`PullConfig::solve_slots`] handlers enter
    /// the CPU-bound solve stage concurrently. On shutdown it stops fetching,
    /// waits up to [`PullConfig::grace`] for in-flight handlers, then returns
    /// the run's [`PullStats`].
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
            metrics,
        } = self;

        let region = Arc::new(region);
        let cells = Arc::new(cells);
        let solve_limiter = Arc::new(SolveLimiter::new(cfg.solve_slots));

        metrics.graph_ready(region.id.as_str(), 1);
        metrics.matcher_handlers_in_flight(region.id.as_str(), 0);
        metrics.matcher_solves_in_flight(region.id.as_str(), 0);

        let mut stats = PullStats::default();
        let mut in_flight = FuturesUnordered::new();
        let refill_at = cfg
            .max_batch
            .max(1)
            .min(cfg.max_in_flight.get())
            .div_ceil(2);

        while !shutdown.is_triggered() {
            let free = cfg.max_in_flight.get().saturating_sub(in_flight.len());
            if free < refill_at {
                // Retain enough live handlers to overlap the next broker pull,
                // but amortise that pull over more than one completion.
                tokio::select! {
                    biased;
                    () = shutdown.triggered() => break,
                    Some(done) = in_flight.next() => {
                        record(done, &mut stats);
                        metrics.matcher_handlers_in_flight(
                            region.id.as_str(),
                            in_flight.len() as u64,
                        );
                    },
                }
                continue;
            }

            let want = free.min(cfg.max_batch.max(1));
            // Keep the same fetch future alive while solves finish. Once a
            // JetStream pull reaches the broker, dropping its future can
            // orphan delivered messages until `AckWait`; polling completions
            // inside this loop overlaps broker I/O without cancelling it.
            let fetch_started = bus::wallclock();
            let fetch = consumer.fetch(want, cfg.fetch_wait);
            tokio::pin!(fetch);
            let batch = loop {
                tokio::select! {
                    biased;
                    () = shutdown.triggered() => break None,
                    batch = &mut fetch => break Some(batch),
                    Some(done) = in_flight.next(), if !in_flight.is_empty() => {
                        record(done, &mut stats);
                        metrics.matcher_handlers_in_flight(
                            region.id.as_str(),
                            in_flight.len() as u64,
                        );
                    }
                }
            };
            let Some(batch) = batch else {
                break;
            };
            let fetch_seconds = bus::wallclock()
                .duration_since(fetch_started)
                .map_or(0.0, |elapsed| elapsed.as_secs_f64());
            metrics.job_fetch(
                region.id.as_str(),
                fetch_seconds,
                batch
                    .as_ref()
                    .ok()
                    .map(|deliveries| deliveries.len() as u64),
            );
            match batch {
                Ok(deliveries) => {
                    stats.fetched += deliveries.len() as u64;
                    for delivery in deliveries {
                        in_flight.push(handle_one(
                            delivery,
                            Arc::clone(&engine),
                            publisher.clone(),
                            Arc::clone(&region),
                            Arc::clone(&cells),
                            Arc::clone(&solve_limiter),
                            cfg.validate,
                            metrics.clone(),
                            drain.begin(),
                        ));
                    }
                    metrics.matcher_handlers_in_flight(region.id.as_str(), in_flight.len() as u64);
                }
                Err(error) => warn!(%error, "job fetch failed; retrying"),
            }
        }

        // Shutdown: drain in-flight solves within the `quiesce` grace budget.
        stats.drained = {
            let quiesce = drain.quiesce(cfg.grace);
            tokio::pin!(quiesce);
            loop {
                tokio::select! {
                    biased;
                    Some(done) = in_flight.next(), if !in_flight.is_empty() => {
                        record(done, &mut stats);
                        metrics.matcher_handlers_in_flight(
                            region.id.as_str(),
                            in_flight.len() as u64,
                        );
                    }
                    outcome = &mut quiesce => break matches!(outcome, QuiesceOutcome::Drained),
                }
            }
        };

        drop(in_flight);
        metrics.matcher_handlers_in_flight(region.id.as_str(), 0);
        metrics.matcher_solves_in_flight(region.id.as_str(), 0);
        metrics.graph_ready(region.id.as_str(), 0);

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

/// CPU admission shared by every handler in one pull loop. Its permit guard
/// owns the in-flight gauge transition, including cancellation paths.
struct SolveLimiter {
    semaphore: Arc<Semaphore>,
    active: AtomicUsize,
}

impl SolveLimiter {
    fn new(slots: NonZeroUsize) -> Self {
        Self {
            semaphore: Arc::new(Semaphore::new(slots.get())),
            active: AtomicUsize::new(0),
        }
    }

    async fn acquire(self: &Arc<Self>, metrics: &Metrics, region: &Arc<Region>) -> SolvePermit {
        let permit = Arc::clone(&self.semaphore)
            .acquire_owned()
            .await
            .expect("the matcher never closes its solve semaphore");
        let active = self.active.fetch_add(1, Ordering::Relaxed) + 1;
        metrics.matcher_solves_in_flight(region.id.as_str(), active as u64);
        SolvePermit {
            limiter: Arc::clone(self),
            _permit: permit,
            metrics: metrics.clone(),
            region: Arc::clone(region),
        }
    }
}

/// RAII solve admission so cancellation releases both the semaphore and gauge.
struct SolvePermit {
    limiter: Arc<SolveLimiter>,
    _permit: OwnedSemaphorePermit,
    metrics: Metrics,
    region: Arc<Region>,
}

impl Drop for SolvePermit {
    fn drop(&mut self) {
        let active = self.limiter.active.fetch_sub(1, Ordering::Relaxed) - 1;
        self.metrics
            .matcher_solves_in_flight(self.region.id.as_str(), active as u64);
    }
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
#[allow(clippy::too_many_arguments)]
async fn handle_one<N, H, P>(
    delivery: Delivery<RawBytes, H>,
    engine: Arc<Engine<N>>,
    publisher: ResultPublisher<P>,
    region: Arc<Region>,
    cells: Arc<HashSet<Geohash>>,
    solve_limiter: Arc<SolveLimiter>,
    validate: ValidateConfig,
    metrics: Metrics,
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
    let redelivered = delivery.redelivered;
    let handle = delivery.handle;
    let RawBytes(bytes) = delivery.item;

    // Size-gate then decode; unanswerable bytes are acked and dropped.
    let job = match check_bytes::<N::Entry>(&bytes, &validate) {
        Ok(job) => job,
        Err(reason) => {
            debug!(%reason, "dropping unanswerable job bytes");
            let _ = handle.ack().await;
            return Handled::Poison;
        }
    };

    if let Some(sent) = sent_at {
        let now = bus::wallclock();
        bus::span_between("queue_wait", sent, now);
        if let Ok(waited) = now.duration_since(sent) {
            metrics.queue_wait_seconds(region.id.as_str(), redelivered, waited.as_secs_f64());
        }
    }

    let now_us = unix_micros();
    metrics.freshness_target(region.id.as_str(), job.freshness_target_us, now_us);
    match check(&job, &region, &cells, now_us, &validate) {
        Checked::Refuse(outcome) => {
            let result = SolveResult::new(&job, outcome, now_us);
            match publisher.publish_then_ack(&result, handle).await {
                Ok(published) => {
                    metrics.result_bytes(region.id.as_str(), published.bytes as u64);
                    Handled::Refused
                }
                Err(error) => {
                    warn!(%error, job = %job.id, "failed to publish refusal");
                    Handled::PublishFailed
                }
            }
        }
        // Take id and identity before the job is moved into the blocking solve.
        Checked::Solve(_) => {
            let job_id = job.id;
            let identity = job.identity.clone();
            let proof = job.proof();
            let outcome = {
                // The permit deliberately excludes validation and result I/O:
                // only the CPU-bound blocking solve consumes solve capacity.
                let permit_wait_started = bus::wallclock();
                let _permit = solve_limiter.acquire(&metrics, &region).await;
                if let Ok(elapsed) = bus::wallclock().duration_since(permit_wait_started) {
                    metrics.solve_permit_wait_seconds(region.id.as_str(), elapsed.as_secs_f64());
                }
                let solve_started = bus::wallclock();
                let outcome = engine.solve_blocking(job).await;
                let solved_at = bus::wallclock();
                bus::span_between("solve_seconds", solve_started, solved_at);
                if let Ok(elapsed) = solved_at.duration_since(solve_started) {
                    metrics.solve_seconds(
                        region.id.as_str(),
                        outcome.kind(),
                        elapsed.as_secs_f64(),
                    );
                }
                outcome
            };
            let result = SolveResult {
                job: job_id,
                identity,
                proof,
                outcome,
                solved_at_us: unix_micros(),
            };
            let publish_start = bus::wallclock();
            let published = publisher.publish_then_ack(&result, handle).await;
            if let Ok(elapsed) = bus::wallclock().duration_since(publish_start) {
                metrics.result_publish_seconds(elapsed.as_secs_f64());
            }
            match published {
                Ok(published) => {
                    metrics.result_bytes(region.id.as_str(), published.bytes as u64);
                    Handled::Solved
                }
                Err(error) => {
                    warn!(%error, job = %job_id, "failed to publish solve result");
                    Handled::PublishFailed
                }
            }
        }
        // `check` never yields `Poison`, but the shared verdict type carries it.
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
    use core::num::{NonZeroU8, NonZeroU32};
    use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
    use core::time::Duration;

    use async_nats::HeaderMap;
    use geo::point;
    use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
    use routers_transition::{Continuation, Origin};

    use crate::bus::Wire;
    use crate::bus::adapter::{
        AckHandle, Consumer, Delivery, PublishError, PublishOutcome, Publisher, Source,
    };
    use crate::bus::memory::{MemoryBus, MemoryConsumer, MemoryPublisher};
    use crate::event::{self, VehicleId};
    use crate::lifecycle::{Drain, DrainReason, Shutdown};
    use crate::matcher::engine::Engine;
    use crate::matcher::publish::{PublishConfig, ResultPublisher};
    use crate::protocol::ids::{GraphVersion, Lane, ObservationId, RegionId, SCHEMA_VERSION};
    use crate::protocol::job::{JobIdentity, SolveJob};
    use crate::protocol::result::{SolveOutcome, SolveResult};
    use crate::region::{LaneCount, Region, Replicas};
    use crate::topology::jobs::{job_stream_subjects, job_subject};

    const GRAPH: &str = "v1";
    const REGION: &str = "region";
    /// The result-plane wildcard the tests inspect and drive shutdown from.
    const RESULTS: &str = "solve-result.v1.p.>";

    /// A staircase road (the shared `matched_diff` shape).
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
            lanes: LaneCount::new(NonZeroU8::MIN),
            resource_class: "cpu-1".to_owned(),
            replicas: Replicas::new(NonZeroU32::MIN, NonZeroU32::MIN).expect("range"),
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

    /// A well-formed restart job for `vehicle`, with freshness target `target_us`.
    fn restart_job(vehicle: u64, target_us: i64) -> SolveJob<MockEntryId> {
        SolveJob::new(
            identity(vehicle),
            Lane::DEFAULT,
            target_us,
            Continuation::Restart {
                fresh: observations(),
            },
        )
    }

    /// Tight timings so the tests never sleep for real.
    fn config(max_in_flight: usize) -> PullConfig {
        PullConfig {
            max_in_flight: NonZeroUsize::new(max_in_flight).expect("test capacity is non-zero"),
            solve_slots: NonZeroUsize::new(max_in_flight).expect("test capacity is non-zero"),
            fetch_wait: Duration::from_millis(20),
            max_batch: max_in_flight,
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

    #[derive(Clone)]
    struct BlockingPublisher {
        arrivals: Arc<AtomicUsize>,
        sequence: Arc<AtomicU64>,
        rendezvous: Arc<tokio::sync::Barrier>,
    }

    impl BlockingPublisher {
        fn new(concurrent_publishes: usize) -> Self {
            Self {
                arrivals: Arc::new(AtomicUsize::new(0)),
                sequence: Arc::new(AtomicU64::new(0)),
                rendezvous: Arc::new(tokio::sync::Barrier::new(concurrent_publishes + 1)),
            }
        }
    }

    impl Publisher<SolveResult<MockEntryId>> for BlockingPublisher {
        async fn publish_bytes(
            &self,
            _subject: &str,
            _msg_id: &str,
            _headers: HeaderMap,
            _bytes: &[u8],
        ) -> Result<PublishOutcome, PublishError> {
            self.arrivals.fetch_add(1, Ordering::Relaxed);
            self.rendezvous.wait().await;
            Ok(PublishOutcome::Acked {
                sequence: self.sequence.fetch_add(1, Ordering::Relaxed) + 1,
                duplicate: false,
            })
        }
    }

    #[derive(Default)]
    struct FetchProbe {
        second_started: AtomicBool,
        cancelled: AtomicBool,
        acked: AtomicUsize,
        release_second: tokio::sync::Notify,
    }

    struct ProbeAck {
        sequence: u64,
        probe: Arc<FetchProbe>,
    }

    impl AckHandle for ProbeAck {
        async fn ack(self) -> anyhow::Result<()> {
            self.probe.acked.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn nak(self, _delay: Option<Duration>) -> anyhow::Result<()> {
            Ok(())
        }

        fn sequence(&self) -> u64 {
            self.sequence
        }

        fn deliveries(&self) -> u32 {
            1
        }
    }

    struct CancellationGuard {
        probe: Arc<FetchProbe>,
        armed: bool,
    }

    impl Drop for CancellationGuard {
        fn drop(&mut self) {
            if self.armed {
                self.probe.cancelled.store(true, Ordering::Relaxed);
            }
        }
    }

    struct CancellationSensitiveConsumer {
        calls: usize,
        jobs: [Vec<u8>; 2],
        probe: Arc<FetchProbe>,
    }

    impl CancellationSensitiveConsumer {
        fn new(first: &SolveJob<MockEntryId>, second: &SolveJob<MockEntryId>) -> Self {
            Self {
                calls: 0,
                jobs: [first.encode().unwrap(), second.encode().unwrap()],
                probe: Arc::new(FetchProbe::default()),
            }
        }

        fn delivery(&self, index: usize) -> Delivery<RawBytes, ProbeAck> {
            let sequence = index as u64 + 1;
            Delivery {
                item: RawBytes(self.jobs[index].clone()),
                handle: ProbeAck {
                    sequence,
                    probe: Arc::clone(&self.probe),
                },
                subject: job_subject(
                    &GraphVersion::new(GRAPH).unwrap(),
                    &RegionId::new(REGION).unwrap(),
                    Lane::DEFAULT,
                ),
                msg_id: None,
                headers: HeaderMap::new(),
                sent_at: None,
                redelivered: false,
            }
        }
    }

    impl Consumer<RawBytes> for CancellationSensitiveConsumer {
        type Handle = ProbeAck;

        async fn fetch(
            &mut self,
            _max: usize,
            _wait: Duration,
        ) -> anyhow::Result<Vec<Delivery<RawBytes, Self::Handle>>> {
            let call = self.calls;
            self.calls += 1;
            match call {
                0 => Ok(vec![self.delivery(0)]),
                1 => {
                    let probe = Arc::clone(&self.probe);
                    probe.second_started.store(true, Ordering::Relaxed);
                    let mut guard = CancellationGuard {
                        probe: Arc::clone(&probe),
                        armed: true,
                    };
                    probe.release_second.notified().await;
                    guard.armed = false;
                    Ok(vec![self.delivery(1)])
                }
                _ => {
                    core::future::pending::<()>().await;
                    unreachable!("the pull loop shuts down before a third batch")
                }
            }
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

    #[test]
    fn default_config_separates_handler_and_cpu_capacity() {
        let cfg = PullConfig::default();

        assert_eq!(cfg.max_in_flight, NonZeroUsize::new(32).unwrap());
        assert_eq!(
            cfg.solve_slots,
            std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN)
        );
        assert_eq!(cfg.max_batch, cfg.max_in_flight.get());
    }

    #[tokio::test]
    async fn solves_valid_jobs_and_acks_them() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        publish_job(&bus, &restart_job(1, i64::MAX)).await;
        publish_job(&bus, &restart_job(2, i64::MAX)).await;

        let mut results = bus.source::<SolveResult<MockEntryId>>(RESULTS);
        let pull = pull_loop(&bus, config(4), shutdown.clone());

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
    async fn solve_slots_do_not_serialize_result_io() {
        const HANDLERS: usize = 4;

        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        for vehicle in 1..=HANDLERS as u64 {
            publish_job(&bus, &restart_job(vehicle, i64::MAX)).await;
        }

        let publisher = BlockingPublisher::new(HANDLERS);
        let arrivals = Arc::clone(&publisher.arrivals);
        let rendezvous = Arc::clone(&publisher.rendezvous);
        let mut cfg = config(HANDLERS);
        cfg.solve_slots = NonZeroUsize::MIN;
        let pull = PullLoop::new(
            engine(),
            bus.consumer::<RawBytes>(&job_filter()),
            ResultPublisher::new(publisher, fast_publish()),
            region(),
            served_cells(),
            cfg,
            shutdown.clone(),
            Drain::new(),
        );

        let run = async {
            let driver = async {
                // All handlers must pass through the single solve slot and
                // reach publication before any publisher is released.
                rendezvous.wait().await;
                wait_until(|| bus.acked_count(&job_filter()) == HANDLERS).await;
                shutdown.trigger(DrainReason::Operator);
            };
            tokio::join!(pull.run(), driver).0
        };
        let stats = tokio::time::timeout(Duration::from_secs(2), run)
            .await
            .expect("result I/O was serialised behind the solve permit");

        assert_eq!(arrivals.load(Ordering::Relaxed), HANDLERS);
        assert_eq!(stats.solved, HANDLERS as u64);
        assert!(stats.drained);
    }

    #[tokio::test]
    async fn a_solve_completion_does_not_cancel_an_accepted_fetch() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        let first = restart_job(1, i64::MAX);
        let second = restart_job(2, i64::MAX);
        let consumer = CancellationSensitiveConsumer::new(&first, &second);
        let probe = Arc::clone(&consumer.probe);
        let publisher =
            ResultPublisher::new(bus.publisher::<SolveResult<MockEntryId>>(), fast_publish());
        let pull = PullLoop::new(
            engine(),
            consumer,
            publisher,
            region(),
            served_cells(),
            config(2),
            shutdown.clone(),
            Drain::new(),
        );

        let driver = async {
            wait_until(|| probe.second_started.load(Ordering::Relaxed)).await;
            wait_until(|| probe.acked.load(Ordering::Relaxed) >= 1).await;
            tokio::time::sleep(Duration::from_millis(10)).await;
            assert!(
                !probe.cancelled.load(Ordering::Relaxed),
                "a completion must not drop an already-started broker fetch"
            );
            probe.release_second.notify_one();
            wait_until(|| probe.acked.load(Ordering::Relaxed) == 2).await;
            shutdown.trigger(DrainReason::Operator);
        };

        let (stats, ()) = tokio::join!(pull.run(), driver);
        assert_eq!(stats.fetched, 2);
        assert_eq!(stats.solved, 2);
        assert!(!probe.cancelled.load(Ordering::Relaxed));
    }

    #[tokio::test]
    async fn overdue_job_is_solved_and_acked() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        publish_job(&bus, &restart_job(7, 1)).await;

        let mut results = bus.source::<SolveResult<MockEntryId>>(RESULTS);
        let pull = pull_loop(&bus, config(2), shutdown.clone());

        let driver = async {
            let delivery = results.next().await.expect("a result").expect("it decodes");
            assert!(
                delivery.item.outcome.is_success(),
                "an overdue freshness target must not refuse a valid job, got {:?}",
                delivery.item.outcome
            );
            shutdown.trigger(DrainReason::Operator);
        };

        let (stats, ()) = tokio::join!(pull.run(), driver);

        assert_eq!(stats.refused, 0);
        assert_eq!(stats.solved, 1);
        assert!(stats.drained);
        assert_eq!(bus.acked_count(&job_filter()), 1, "the solved job is acked");
    }

    #[tokio::test]
    async fn oversized_blob_is_acked_without_a_result() {
        let bus = MemoryBus::new();
        let shutdown = Shutdown::new();
        let cfg = PullConfig {
            validate: ValidateConfig {
                max_decoded_bytes: 16,
            },
            ..config(2)
        };

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
        let pull = pull_loop(&bus, config(2), shutdown.clone());

        let driver = async {
            tokio::time::sleep(Duration::from_millis(10)).await;
            shutdown.trigger(DrainReason::Signal);
        };

        let (stats, ()) = tokio::join!(pull.run(), driver);

        assert!(stats.drained, "an idle loop drains immediately");
        assert_eq!(stats.fetched, 0);
        assert_eq!(stats.solved, 0);
    }
}
