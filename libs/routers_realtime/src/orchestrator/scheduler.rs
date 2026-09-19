//! Per-partition vehicle state and the scheduler that sequences work over it.
//!
//! A partition worker owns exactly one [`Scheduler`]. It does no I/O and never
//! reads the clock: every mutating call takes `now: Instant`, so the state
//! machine is deterministic. A vehicle has at most one logical job in flight;
//! per-vehicle order holds because the next observation is not dispatched until
//! the head commits and is popped. Ready vehicles wait in a fair FIFO, and the
//! `committing` flag arbitrates — a prepared commit always wins over a timeout.

use alloc::collections::VecDeque;
use core::time::Duration;
use std::collections::{HashMap, HashSet};

use routers_network::Entry;
use thiserror::Error;
use tokio::time::Instant;

use crate::bus::adapter::AckHandle;
use crate::event::{Payload, VehicleId};
use crate::orchestrator::admission;
use crate::protocol::ids::{JobId, ObservationId};
use crate::protocol::job::JobIdentity;
use crate::protocol::result::SolveResult;
use crate::store::checkpoint::VehicleCheckpoint;

/// What the scheduler knows about a vehicle's committed checkpoint.
///
/// Only a loaded checkpoint lets [`Scheduler::enqueue`] suppress an
/// already-committed observation, so `Unloaded` is never treated as `Absent`.
// Boxing `Present` would add an indirection on the common checkpoint read.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum CheckpointState<E: Entry> {
    /// The store has not been consulted yet.
    Unloaded,
    /// The store was consulted and this vehicle has no committed checkpoint.
    Absent,
    /// The last committed checkpoint for this vehicle.
    Present(VehicleCheckpoint<E>),
}

impl<E: Entry> CheckpointState<E> {
    /// The committed checkpoint if one is loaded and present.
    #[must_use]
    pub fn present(&self) -> Option<&VehicleCheckpoint<E>> {
        match self {
            CheckpointState::Present(cp) => Some(cp),
            CheckpointState::Unloaded | CheckpointState::Absent => None,
        }
    }

    /// Whether the store has been consulted for this vehicle (either answer).
    #[must_use]
    pub fn is_loaded(&self) -> bool {
        !matches!(self, CheckpointState::Unloaded)
    }
}

/// One observation queued for a vehicle, together with the ack handle that
/// retires its raw delivery.
///
/// The observation is not acknowledged until the work it drives has committed
/// (or it is coalesced / suppressed / overflowed and the handle travels back
/// out in the [`Enqueue`] result).
pub struct PendingObservation<H: AckHandle> {
    /// The observation's durable identity `(partition, sequence)`.
    pub id: ObservationId,
    /// The decoded observation payload.
    pub payload: Payload,
    /// The handle that acknowledges this observation's raw delivery.
    pub handle: H,
    /// When the worker received it — used to measure queue-wait latency.
    pub received: Instant,
}

/// Why an active scheduler entry does or does not own admission credit.
#[derive(Debug)]
pub enum JobReservation {
    /// A published matcher job owns its normal admission permit.
    Admitted(admission::Permit),
    /// A worker-local terminal transition publishes no matcher job and needs no
    /// admission credit.
    SyntheticTerminal,
}

/// The single logical solve a vehicle has in flight.
///
/// It owns the [`admission::Permit`], released when the job is dropped on
/// [`Scheduler::finish`] or [`Scheduler::abandon`].
#[derive(Debug)]
pub struct ActiveJob {
    /// The job's deterministic identity (its broker dedup key).
    pub id: JobId,
    /// The full job context, kept so a result can be validated without a lookup.
    pub identity: JobIdentity,
    /// The head observation this job is solving.
    pub observation: ObservationId,
    /// When the job's deadline fires (a monotonic instant, not wall-clock).
    pub deadline: Instant,
    /// The job's encoded size in bytes — what the admission permit reserved.
    pub bytes: u64,
    /// The admission state held for the lifetime of this scheduler entry.
    pub reservation: JobReservation,
    /// When the job was dispatched — used to measure solve latency.
    pub dispatched: Instant,
}

/// Everything a partition worker remembers about one vehicle.
/// A point-in-time count of what one partition's scheduler is holding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Depth {
    pub vehicles: usize,
    pub pending: usize,
    pub active: usize,
}

pub struct VehicleState<E: Entry, H: AckHandle> {
    /// The loaded (or not-yet-loaded) committed checkpoint.
    pub checkpoint: CheckpointState<E>,
    /// FIFO of observations awaiting a solve; the front is the head.
    pub pending: VecDeque<PendingObservation<H>>,
    /// The one logical job in flight, if any.
    pub active: Option<ActiveJob>,
    /// `true` while a commit is being prepared/published; the vehicle is never
    /// evicted and a later deadline defers to the commit while this is set.
    pub committing: bool,
    /// Results that arrived before the state that should consume them.
    pub parked: Vec<SolveResult<E>>,
    /// The last time the vehicle saw activity — drives idle eviction.
    pub last_touch: Instant,
}

impl<E: Entry, H: AckHandle> VehicleState<E, H> {
    /// A brand-new, empty vehicle first seen at `now`.
    fn new(now: Instant) -> Self {
        Self {
            checkpoint: CheckpointState::Unloaded,
            pending: VecDeque::new(),
            active: None,
            committing: false,
            parked: Vec::new(),
            last_touch: now,
        }
    }

    /// Ready to dispatch: has a head, no active job, and not committing.
    fn is_eligible(&self) -> bool {
        self.active.is_none() && !self.committing && !self.pending.is_empty()
    }

    /// Reclaimable: no pending work, no active job, not committing.
    fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.active.is_none() && !self.committing
    }
}

/// The outcome of offering an observation to the scheduler.
///
/// Three of the four variants hand the ack handle back so the caller can
/// acknowledge the raw delivery immediately rather than leaking it.
#[must_use = "an enqueue result may carry an ack handle that must be acknowledged"]
pub enum Enqueue<H: AckHandle> {
    /// The observation was appended to the vehicle's FIFO; `depth` is the new
    /// queue length including this observation.
    Queued {
        /// The vehicle's pending depth after the append.
        depth: usize,
    },
    /// A duplicate pending/active delivery; the handle is returned to be acked.
    Coalesced(H),
    /// At or below the last committed input; the handle is returned to be acked.
    Committed(H),
    /// The FIFO is at [`SchedulerConfig::pending_limit`]; the handle is returned.
    Overflow {
        /// The refused observation's ack handle.
        handle: H,
        /// The per-vehicle pending limit that was hit.
        limit: usize,
    },
}

/// The outcome of parking an early result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Park {
    /// The result was stored for later replay.
    Parked,
    /// The buffer was full or the vehicle untracked; the result was dropped.
    Dropped {
        /// The park-buffer bound that was hit.
        limit: usize,
    },
}

/// The head observation and active job handed back by [`Scheduler::finish`].
pub struct Finished<H: AckHandle> {
    /// The head observation just popped from the FIFO.
    pub observation: PendingObservation<H>,
    /// The committed active job; dropping it releases the admission permit.
    pub job: ActiveJob,
}

/// A point-in-time count of what the scheduler is holding.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SchedulerStats {
    /// Number of tracked vehicles.
    pub vehicles: usize,
    /// Total pending observations across all vehicles.
    pub pending: usize,
    /// Number of vehicles with an active job.
    pub active: usize,
    /// Number of vehicles currently queued as ready.
    pub ready: usize,
    /// Total parked results across all vehicles.
    pub parked: usize,
    /// Number of vehicles with a commit in flight.
    pub committing: usize,
}

/// Static limits for a [`Scheduler`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerConfig {
    /// Most early results parked per vehicle before further ones are dropped.
    pub parked_limit: usize,
    /// How long a vehicle may sit idle before [`Scheduler::evict_idle`] reclaims it.
    pub idle_ttl: Duration,
    /// Most observations a vehicle may hold pending before further ones overflow.
    pub pending_limit: usize,
}

impl Default for SchedulerConfig {
    /// 4 parked results, a 10-minute idle TTL, 256 pending per vehicle.
    fn default() -> Self {
        Self {
            parked_limit: 4,
            idle_ttl: Duration::from_secs(10 * 60),
            pending_limit: 256,
        }
    }
}

/// Why a scheduler state transition was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Error)]
pub enum SchedulerError {
    /// The vehicle is not tracked by this scheduler.
    #[error("vehicle is not tracked by the scheduler")]
    UnknownVehicle,
    /// [`Scheduler::activate`] on a vehicle that already has an active job.
    #[error("vehicle already has an active job")]
    AlreadyActive,
    /// [`Scheduler::begin_commit`]/[`Scheduler::finish`] with no active job.
    #[error("vehicle has no active job")]
    NoActiveJob,
    /// [`Scheduler::begin_commit`] where a prepared commit beat a later timeout.
    #[error("vehicle is already committing")]
    AlreadyCommitting,
    /// [`Scheduler::finish`] found no head to pop — an invariant violation.
    #[error("vehicle has no head observation to finish")]
    NoHead,
}

/// The per-partition vehicle table and ready queue.
///
/// Generic over the entry type `E` and the ack-handle type `H`. A partition
/// worker owns one of these and never shares it.
pub struct Scheduler<E: Entry, H: AckHandle> {
    config: SchedulerConfig,
    vehicles: HashMap<VehicleId, VehicleState<E, H>>,
    ready: VecDeque<VehicleId>,
    ready_set: HashSet<VehicleId>,
    /// A standing `Unloaded` so [`Scheduler::checkpoint`] can be total.
    absent_checkpoint: CheckpointState<E>,
}

impl<E: Entry, H: AckHandle> Scheduler<E, H> {
    /// Build an empty scheduler with the given limits.
    #[must_use]
    pub fn new(config: SchedulerConfig) -> Self {
        Self {
            config,
            vehicles: HashMap::new(),
            ready: VecDeque::new(),
            ready_set: HashSet::new(),
            absent_checkpoint: CheckpointState::Unloaded,
        }
    }

    /// The configured limits.
    #[must_use]
    pub fn config(&self) -> &SchedulerConfig {
        &self.config
    }

    /// Queue a ready vehicle, guarding against a double entry so the FIFO holds
    /// each vehicle at most once.
    fn mark_ready(&mut self, vehicle: VehicleId) {
        if self.ready_set.insert(vehicle) {
            self.ready.push_back(vehicle);
        }
    }

    /// Offer an observation to its vehicle's FIFO, creating the vehicle if new.
    ///
    /// Returns [`Enqueue::Committed`] if a loaded checkpoint already covers it,
    /// [`Enqueue::Coalesced`] for a duplicate pending/active id,
    /// [`Enqueue::Overflow`] if the FIFO is full, else [`Enqueue::Queued`]. A
    /// vehicle that becomes eligible is added to the ready queue.
    pub fn enqueue(&mut self, obs: PendingObservation<H>, now: Instant) -> Enqueue<H> {
        let vehicle = obs.payload.vehicle_id;
        let limit = self.config.pending_limit;
        let state = self
            .vehicles
            .entry(vehicle)
            .or_insert_with(|| VehicleState::new(now));
        state.last_touch = now;

        // Suppression is only decidable once a checkpoint is loaded.
        if let CheckpointState::Present(cp) = &state.checkpoint
            && obs.id.sequence <= cp.last_input.sequence
        {
            return Enqueue::Committed(obs.handle);
        }

        if state
            .active
            .as_ref()
            .is_some_and(|a| a.observation == obs.id)
            || state.pending.iter().any(|p| p.id == obs.id)
        {
            return Enqueue::Coalesced(obs.handle);
        }

        if state.pending.len() >= limit {
            return Enqueue::Overflow {
                handle: obs.handle,
                limit,
            };
        }

        state.pending.push_back(obs);
        let depth = state.pending.len();
        let eligible = state.is_eligible();
        if eligible {
            self.mark_ready(vehicle);
        }
        Enqueue::Queued { depth }
    }

    /// Pop the next ready vehicle in FIFO order, or `None` if none is ready.
    ///
    /// Entries are re-checked for eligibility on the way out, so a stale entry
    /// is skipped rather than returned.
    pub fn next_ready(&mut self) -> Option<VehicleId> {
        while let Some(vehicle) = self.ready.pop_front() {
            self.ready_set.remove(&vehicle);
            if self
                .vehicles
                .get(&vehicle)
                .is_some_and(|state| state.is_eligible())
            {
                return Some(vehicle);
            }
        }
        None
    }

    /// The vehicle's head observation, if it has one.
    #[must_use]
    pub fn head(&self, vehicle: VehicleId) -> Option<&PendingObservation<H>> {
        self.vehicles.get(&vehicle).and_then(|s| s.pending.front())
    }

    /// The vehicle's checkpoint state, or a standing `Unloaded` if untracked.
    #[must_use]
    pub fn checkpoint(&self, vehicle: VehicleId) -> &CheckpointState<E> {
        self.vehicles
            .get(&vehicle)
            .map_or(&self.absent_checkpoint, |s| &s.checkpoint)
    }

    /// Record a loaded checkpoint for a tracked vehicle; a no-op if untracked.
    pub fn set_checkpoint(&mut self, vehicle: VehicleId, checkpoint: CheckpointState<E>) {
        if let Some(state) = self.vehicles.get_mut(&vehicle) {
            state.checkpoint = checkpoint;
        }
    }

    /// Attach the one logical job to a vehicle taken from [`Scheduler::next_ready`].
    ///
    /// Errors [`AlreadyActive`](SchedulerError::AlreadyActive) if a job is
    /// attached, or [`UnknownVehicle`](SchedulerError::UnknownVehicle) if untracked.
    pub fn activate(&mut self, vehicle: VehicleId, job: ActiveJob) -> Result<(), SchedulerError> {
        let state = self
            .vehicles
            .get_mut(&vehicle)
            .ok_or(SchedulerError::UnknownVehicle)?;
        if state.active.is_some() {
            return Err(SchedulerError::AlreadyActive);
        }
        state.active = Some(job);
        Ok(())
    }

    /// The full [`VehicleState`] for a tracked vehicle, or `None` if untracked.
    #[must_use]
    pub fn state(&self, vehicle: VehicleId) -> Option<&VehicleState<E, H>> {
        self.vehicles.get(&vehicle)
    }

    /// The vehicle's active job, if any.
    #[must_use]
    pub fn active(&self, vehicle: VehicleId) -> Option<&ActiveJob> {
        self.vehicles.get(&vehicle).and_then(|s| s.active.as_ref())
    }

    /// The vehicle's active job for in-place update (e.g. re-arming a deadline).
    pub fn active_mut(&mut self, vehicle: VehicleId) -> Option<&mut ActiveJob> {
        self.vehicles
            .get_mut(&vehicle)
            .and_then(|s| s.active.as_mut())
    }

    /// Set the arbitration flag: a commit is in flight for the active job.
    ///
    /// Errors [`NoActiveJob`](SchedulerError::NoActiveJob) if nothing to commit,
    /// or [`AlreadyCommitting`](SchedulerError::AlreadyCommitting) when a
    /// prepared commit has already beaten a timeout.
    pub fn begin_commit(&mut self, vehicle: VehicleId) -> Result<(), SchedulerError> {
        let state = self
            .vehicles
            .get_mut(&vehicle)
            .ok_or(SchedulerError::UnknownVehicle)?;
        if state.active.is_none() {
            return Err(SchedulerError::NoActiveJob);
        }
        if state.committing {
            return Err(SchedulerError::AlreadyCommitting);
        }
        state.committing = true;
        Ok(())
    }

    /// Complete the active job: pop the head, take the job, clear the committing
    /// flag, and re-ready the vehicle if pending work remains.
    ///
    /// The terminal move of every committed outcome. Errors
    /// [`NoActiveJob`](SchedulerError::NoActiveJob) if no job is attached.
    pub fn finish(
        &mut self,
        vehicle: VehicleId,
        now: Instant,
    ) -> Result<Finished<H>, SchedulerError> {
        let state = self
            .vehicles
            .get_mut(&vehicle)
            .ok_or(SchedulerError::UnknownVehicle)?;
        let job = state.active.take().ok_or(SchedulerError::NoActiveJob)?;
        let Some(observation) = state.pending.pop_front() else {
            // An active job always has its head in the FIFO; restore and report.
            state.active = Some(job);
            return Err(SchedulerError::NoHead);
        };
        state.committing = false;
        state.last_touch = now;
        let eligible = state.is_eligible();
        if eligible {
            self.mark_ready(vehicle);
        }
        Ok(Finished { observation, job })
    }

    /// Drop the active job **without** popping the head, returning it so the
    /// caller can release its permit; the same observation can re-dispatch.
    ///
    /// The retry path, distinct from [`Scheduler::finish`] which advances past
    /// the head. Returns `None` if untracked or with no active job.
    pub fn abandon(&mut self, vehicle: VehicleId) -> Option<ActiveJob> {
        let state = self.vehicles.get_mut(&vehicle)?;
        let job = state.active.take()?;
        let eligible = state.is_eligible();
        if eligible {
            self.mark_ready(vehicle);
        }
        Some(job)
    }

    /// Park an early result for later replay, up to
    /// [`SchedulerConfig::parked_limit`]; else [`Park::Dropped`].
    pub fn park(&mut self, vehicle: VehicleId, result: SolveResult<E>, now: Instant) -> Park {
        let limit = self.config.parked_limit;
        let Some(state) = self.vehicles.get_mut(&vehicle) else {
            return Park::Dropped { limit };
        };
        if state.parked.len() >= limit {
            return Park::Dropped { limit };
        }
        state.parked.push(result);
        state.last_touch = now;
        Park::Parked
    }

    /// Drain and return the vehicle's parked results (empty if none/untracked).
    pub fn take_parked(&mut self, vehicle: VehicleId) -> Vec<SolveResult<E>> {
        self.vehicles
            .get_mut(&vehicle)
            .map(|s| core::mem::take(&mut s.parked))
            .unwrap_or_default()
    }

    /// Remove and return every idle vehicle whose `last_touch` has aged past
    /// [`SchedulerConfig::idle_ttl`].
    pub fn evict_idle(&mut self, now: Instant) -> Vec<VehicleId> {
        let ttl = self.config.idle_ttl;
        let mut evicted = Vec::new();
        self.vehicles.retain(|&vehicle, state| {
            let expired = now.saturating_duration_since(state.last_touch) >= ttl;
            if state.is_idle() && expired {
                evicted.push(vehicle);
                false
            } else {
                true
            }
        });
        evicted
    }

    /// A point-in-time count of what the scheduler is holding.
    #[must_use]
    pub fn stats(&self) -> SchedulerStats {
        let mut stats = SchedulerStats {
            vehicles: self.vehicles.len(),
            ready: self.ready_set.len(),
            ..SchedulerStats::default()
        };
        for state in self.vehicles.values() {
            stats.pending += state.pending.len();
            stats.parked += state.parked.len();
            if state.active.is_some() {
                stats.active += 1;
            }
            if state.committing {
                stats.committing += 1;
            }
        }
        stats
    }

    /// The age of the oldest head observation, or `None` if nothing is pending.
    #[must_use]
    pub fn oldest_pending(&self, now: Instant) -> Option<Duration> {
        self.vehicles
            .values()
            .filter_map(|s| s.pending.front())
            .map(|p| p.received)
            .min()
            .map(|oldest| now.saturating_duration_since(oldest))
    }

    #[must_use]
    pub fn depth(&self) -> Depth {
        Depth {
            vehicles: self.vehicles.len(),
            pending: self.vehicles.values().map(|s| s.pending.len()).sum(),
            active: self
                .vehicles
                .values()
                .filter(|s| s.active.is_some())
                .count(),
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::sync::Arc;
    use std::sync::Mutex;

    use chrono::{DateTime, Utc};
    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::Trip;

    use super::*;
    use crate::orchestrator::admission::{Admission, AdmissionConfig};
    use crate::protocol::ids::{GraphVersion, RegionId, Revision, SCHEMA_VERSION, SegmentId};
    use crate::protocol::result::SolveOutcome;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum AckOp {
        Ack(u64),
        Nak(u64),
    }

    #[derive(Clone)]
    struct TestAck {
        seq: u64,
        log: Arc<Mutex<Vec<AckOp>>>,
    }

    impl AckHandle for TestAck {
        async fn ack(self) -> anyhow::Result<()> {
            self.log.lock().unwrap().push(AckOp::Ack(self.seq));
            Ok(())
        }

        async fn nak(self, _delay: Option<Duration>) -> anyhow::Result<()> {
            self.log.lock().unwrap().push(AckOp::Nak(self.seq));
            Ok(())
        }

        fn sequence(&self) -> u64 {
            self.seq
        }

        fn deliveries(&self) -> u32 {
            1
        }
    }

    struct Fixture {
        log: Arc<Mutex<Vec<AckOp>>>,
        admission: Admission,
        region: RegionId,
        base: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            let region = RegionId::new("r1").unwrap();
            let admission = Admission::new(AdmissionConfig::default(), core::iter::once(&region));
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                admission,
                region,
                base: Instant::now(),
            }
        }

        fn at(&self, secs: u64) -> Instant {
            self.base + Duration::from_secs(secs)
        }

        fn obs(&self, vehicle: u64, seq: u64, received: Instant) -> PendingObservation<TestAck> {
            PendingObservation {
                id: ObservationId {
                    partition: 7,
                    sequence: seq,
                },
                payload: Payload {
                    vehicle_id: VehicleId(vehicle),
                    timestamp: DateTime::<Utc>::from_timestamp(0, 0).unwrap(),
                    point: Point::new(0.0, 0.0),
                },
                handle: TestAck {
                    seq,
                    log: self.log.clone(),
                },
                received,
            }
        }

        fn identity(&self, vehicle: u64, seq: u64) -> JobIdentity {
            JobIdentity {
                schema: SCHEMA_VERSION,
                vehicle_id: VehicleId(vehicle),
                observation: ObservationId {
                    partition: 7,
                    sequence: seq,
                },
                base: None,
                graph: GraphVersion::new("g1").unwrap(),
                region: self.region.clone(),
            }
        }

        fn job(&self, vehicle: u64, seq: u64, now: Instant) -> ActiveJob {
            let identity = self.identity(vehicle, seq);
            let id = identity.job_id();
            ActiveJob {
                id,
                identity,
                observation: ObservationId {
                    partition: 7,
                    sequence: seq,
                },
                deadline: now + Duration::from_secs(30),
                bytes: 100,
                reservation: JobReservation::Admitted(
                    self.admission.try_admit(&self.region, 100).unwrap(),
                ),
                dispatched: now,
            }
        }

        fn result(&self, vehicle: u64, seq: u64) -> SolveResult<MockEntryId> {
            let identity = self.identity(vehicle, seq);
            let job = identity.job_id();
            SolveResult {
                job,
                identity,
                outcome: SolveOutcome::Unanchored,
                solved_at_us: 0,
            }
        }

        fn present_checkpoint(&self, last_seq: u64) -> CheckpointState<MockEntryId> {
            CheckpointState::Present(VehicleCheckpoint {
                trip: Trip::new(),
                last_input: ObservationId {
                    partition: 7,
                    sequence: last_seq,
                },
                revision: Revision(last_seq),
                segment: SegmentId(1),
                finalized_through: None,
                graph: GraphVersion::new("g1").unwrap(),
                schema: SCHEMA_VERSION,
                region: self.region.clone(),
            })
        }
    }

    fn scheduler(_fx: &Fixture) -> Scheduler<MockEntryId, TestAck> {
        Scheduler::new(SchedulerConfig::default())
    }

    #[track_caller]
    fn feed(
        sched: &mut Scheduler<MockEntryId, TestAck>,
        obs: PendingObservation<TestAck>,
        now: Instant,
    ) {
        assert!(matches!(sched.enqueue(obs, now), Enqueue::Queued { .. }));
    }

    #[track_caller]
    fn expect_queued(result: Enqueue<TestAck>) -> usize {
        match result {
            Enqueue::Queued { depth } => depth,
            Enqueue::Coalesced(_) => panic!("expected Queued, got Coalesced"),
            Enqueue::Committed(_) => panic!("expected Queued, got Committed"),
            Enqueue::Overflow { .. } => panic!("expected Queued, got Overflow"),
        }
    }

    #[test]
    fn fifo_is_preserved_per_vehicle() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        assert_eq!(expect_queued(sched.enqueue(fx.obs(1, 10, now), now)), 1);
        assert_eq!(expect_queued(sched.enqueue(fx.obs(1, 11, now), now)), 2);
        assert_eq!(expect_queued(sched.enqueue(fx.obs(1, 12, now), now)), 3);

        assert_eq!(sched.head(VehicleId(1)).unwrap().id.sequence, 10);

        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
        assert_eq!(sched.next_ready(), None);

        sched.activate(VehicleId(1), fx.job(1, 10, now)).unwrap();
        let finished = sched.finish(VehicleId(1), now).unwrap();
        assert_eq!(finished.observation.id.sequence, 10);
        assert_eq!(sched.head(VehicleId(1)).unwrap().id.sequence, 11);

        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
    }

    #[test]
    fn ready_queue_is_fair_across_vehicles() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 1, now), now);
        feed(&mut sched, fx.obs(2, 1, now), now);
        feed(&mut sched, fx.obs(3, 1, now), now);

        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
        assert_eq!(sched.next_ready(), Some(VehicleId(2)));
        assert_eq!(sched.next_ready(), Some(VehicleId(3)));
        assert_eq!(sched.next_ready(), None);

        sched.activate(VehicleId(1), fx.job(1, 1, now)).unwrap();
        feed(&mut sched, fx.obs(1, 2, now), now);
        feed(&mut sched, fx.obs(2, 2, now), now);
        sched.finish(VehicleId(1), now).unwrap();

        assert_eq!(sched.next_ready(), Some(VehicleId(2)));
        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
    }

    #[test]
    fn duplicate_pending_observation_is_coalesced() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 5, now), now);
        let again = sched.enqueue(fx.obs(1, 5, now), now);
        match again {
            Enqueue::Coalesced(handle) => assert_eq!(handle.sequence(), 5),
            _ => panic!("expected Coalesced for a duplicate pending id"),
        }
        assert_eq!(sched.stats().pending, 1);
    }

    #[test]
    fn duplicate_active_observation_is_coalesced() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 5, now), now);
        sched.next_ready();
        sched.activate(VehicleId(1), fx.job(1, 5, now)).unwrap();

        let again = sched.enqueue(fx.obs(1, 5, now), now);
        match again {
            Enqueue::Coalesced(handle) => assert_eq!(handle.sequence(), 5),
            _ => panic!("expected Coalesced for a duplicate active id"),
        }
        assert_eq!(sched.stats().pending, 1);
    }

    #[test]
    fn already_committed_observation_is_suppressed() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 21, now), now);
        sched.set_checkpoint(VehicleId(1), fx.present_checkpoint(20));

        match sched.enqueue(fx.obs(1, 20, now), now) {
            Enqueue::Committed(handle) => assert_eq!(handle.sequence(), 20),
            _ => panic!("expected Committed at the boundary"),
        }
        match sched.enqueue(fx.obs(1, 19, now), now) {
            Enqueue::Committed(handle) => assert_eq!(handle.sequence(), 19),
            _ => panic!("expected Committed below the boundary"),
        }
        assert!(matches!(
            sched.enqueue(fx.obs(1, 22, now), now),
            Enqueue::Queued { .. }
        ));
    }

    #[test]
    fn suppression_needs_a_loaded_checkpoint() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        assert!(matches!(
            sched.enqueue(fx.obs(1, 1, now), now),
            Enqueue::Queued { .. }
        ));
    }

    #[test]
    fn pending_overflow_refuses_and_returns_the_handle() {
        let fx = Fixture::new();
        let mut sched = Scheduler::<MockEntryId, TestAck>::new(SchedulerConfig {
            pending_limit: 2,
            ..SchedulerConfig::default()
        });
        let now = fx.at(0);

        assert!(matches!(
            sched.enqueue(fx.obs(1, 1, now), now),
            Enqueue::Queued { depth: 1 }
        ));
        assert!(matches!(
            sched.enqueue(fx.obs(1, 2, now), now),
            Enqueue::Queued { depth: 2 }
        ));
        match sched.enqueue(fx.obs(1, 3, now), now) {
            Enqueue::Overflow { handle, limit } => {
                assert_eq!(handle.sequence(), 3);
                assert_eq!(limit, 2);
            }
            _ => panic!("expected Overflow at the limit"),
        }
        assert_eq!(sched.stats().pending, 2);
    }

    #[test]
    fn activate_twice_is_rejected() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 1, now), now);
        sched.next_ready();
        sched.activate(VehicleId(1), fx.job(1, 1, now)).unwrap();
        assert_eq!(
            sched.activate(VehicleId(1), fx.job(1, 1, now)),
            Err(SchedulerError::AlreadyActive)
        );
    }

    #[test]
    fn activate_unknown_vehicle_is_rejected() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);
        assert_eq!(
            sched.activate(VehicleId(99), fx.job(99, 1, now)),
            Err(SchedulerError::UnknownVehicle)
        );
    }

    #[test]
    fn begin_commit_transitions_and_errors() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 1, now), now);
        assert_eq!(
            sched.begin_commit(VehicleId(1)),
            Err(SchedulerError::NoActiveJob)
        );

        sched.next_ready();
        sched.activate(VehicleId(1), fx.job(1, 1, now)).unwrap();
        assert_eq!(sched.begin_commit(VehicleId(1)), Ok(()));
        assert_eq!(
            sched.begin_commit(VehicleId(1)),
            Err(SchedulerError::AlreadyCommitting)
        );
    }

    #[test]
    fn finish_without_active_job_is_rejected() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);
        feed(&mut sched, fx.obs(1, 1, now), now);
        assert!(matches!(
            sched.finish(VehicleId(1), now),
            Err(SchedulerError::NoActiveJob)
        ));
    }

    #[test]
    fn finish_pops_head_and_clears_committing() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 1, now), now);
        sched.next_ready();
        sched.activate(VehicleId(1), fx.job(1, 1, now)).unwrap();
        sched.begin_commit(VehicleId(1)).unwrap();

        let finished = sched.finish(VehicleId(1), now).unwrap();
        assert_eq!(finished.observation.id.sequence, 1);
        assert_eq!(finished.job.observation.sequence, 1);

        let stats = sched.stats();
        assert_eq!(stats.active, 0);
        assert_eq!(stats.committing, 0);
        assert_eq!(stats.pending, 0);
        assert_eq!(sched.next_ready(), None);
    }

    #[test]
    fn abandon_keeps_the_head_and_re_readies() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 1, now), now);
        sched.next_ready();
        sched.activate(VehicleId(1), fx.job(1, 1, now)).unwrap();

        let job = sched.abandon(VehicleId(1)).expect("a job to abandon");
        assert_eq!(job.observation.sequence, 1);
        assert_eq!(sched.head(VehicleId(1)).unwrap().id.sequence, 1);
        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
        assert!(sched.abandon(VehicleId(1)).is_none());
    }

    #[test]
    fn parking_is_bounded_and_drainable() {
        let fx = Fixture::new();
        let mut sched = Scheduler::<MockEntryId, TestAck>::new(SchedulerConfig {
            parked_limit: 2,
            ..SchedulerConfig::default()
        });
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 1, now), now);
        assert_eq!(sched.park(VehicleId(1), fx.result(1, 2), now), Park::Parked);
        assert_eq!(sched.park(VehicleId(1), fx.result(1, 3), now), Park::Parked);
        assert_eq!(
            sched.park(VehicleId(1), fx.result(1, 4), now),
            Park::Dropped { limit: 2 }
        );

        let drained = sched.take_parked(VehicleId(1));
        assert_eq!(drained.len(), 2);
        assert_eq!(sched.park(VehicleId(1), fx.result(1, 5), now), Park::Parked);
    }

    #[test]
    fn parking_an_untracked_vehicle_drops() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);
        assert_eq!(
            sched.park(VehicleId(1), fx.result(1, 1), now),
            Park::Dropped { limit: 4 }
        );
        assert!(sched.take_parked(VehicleId(1)).is_empty());
    }

    #[test]
    fn eviction_only_reclaims_idle_vehicles() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let ttl = sched.config().idle_ttl;

        // 1 pending, 2 active, 3 committing, 4 idle: only 4 is reclaimable.
        let t0 = fx.at(0);
        feed(&mut sched, fx.obs(1, 1, t0), t0);

        feed(&mut sched, fx.obs(2, 1, t0), t0);
        sched.next_ready();
        sched.next_ready();
        sched.activate(VehicleId(2), fx.job(2, 1, t0)).unwrap();

        feed(&mut sched, fx.obs(3, 1, t0), t0);
        sched.next_ready();
        sched.activate(VehicleId(3), fx.job(3, 1, t0)).unwrap();
        sched.begin_commit(VehicleId(3)).unwrap();

        feed(&mut sched, fx.obs(4, 1, t0), t0);
        sched.next_ready();
        sched.activate(VehicleId(4), fx.job(4, 1, t0)).unwrap();
        sched.finish(VehicleId(4), t0).unwrap();

        assert!(sched.evict_idle(t0).is_empty());

        let after = t0 + ttl;
        let evicted = sched.evict_idle(after);
        assert_eq!(evicted, vec![VehicleId(4)]);

        let stats = sched.stats();
        assert_eq!(stats.vehicles, 3);
        assert_eq!(stats.pending, 3);
    }

    #[test]
    fn oldest_pending_picks_the_oldest_head() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);

        feed(&mut sched, fx.obs(1, 1, fx.at(0)), fx.at(0));
        feed(&mut sched, fx.obs(1, 2, fx.at(20)), fx.at(20));
        feed(&mut sched, fx.obs(2, 1, fx.at(5)), fx.at(5));

        let now = fx.at(30);
        assert_eq!(sched.oldest_pending(now), Some(Duration::from_secs(30)));

        let empty = Scheduler::<MockEntryId, TestAck>::new(SchedulerConfig::default());
        assert_eq!(empty.oldest_pending(now), None);
    }

    #[test]
    fn checkpoint_accessor_is_total() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        assert!(matches!(
            sched.checkpoint(VehicleId(1)),
            CheckpointState::Unloaded
        ));

        feed(&mut sched, fx.obs(1, 1, now), now);
        sched.set_checkpoint(VehicleId(1), CheckpointState::Absent);
        assert!(matches!(
            sched.checkpoint(VehicleId(1)),
            CheckpointState::Absent
        ));

        sched.set_checkpoint(VehicleId(2), CheckpointState::Absent);
        assert!(matches!(
            sched.checkpoint(VehicleId(2)),
            CheckpointState::Unloaded
        ));
    }

    #[test]
    fn returned_handle_can_be_acknowledged() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        feed(&mut sched, fx.obs(1, 5, now), now);
        let Enqueue::Coalesced(handle) = sched.enqueue(fx.obs(1, 5, now), now) else {
            panic!("expected Coalesced");
        };
        futures::executor::block_on(handle.ack()).unwrap();
        assert_eq!(&*fx.log.lock().unwrap(), &[AckOp::Ack(5)]);
    }
}
