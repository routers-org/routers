//! Per-partition vehicle state and the scheduler that sequences work over it
//! (spec §3, §8).
//!
//! A partition worker owns exactly one [`Scheduler`]. It is the worker's local
//! map of every vehicle the partition has seen recently, plus the machinery
//! that decides *which vehicle to dispatch next*. Nothing here does I/O or
//! touches the clock: every mutating call takes the current `now: Instant` from
//! the worker, so the whole state machine is deterministic and unit-testable
//! without a runtime. The worker drives it; this module only remembers.
//!
//! # One logical job per vehicle
//!
//! The spec fixes a hard rule: a vehicle has at most one *logical* solve in
//! flight. Observations for a vehicle queue in a per-vehicle FIFO
//! ([`VehicleState::pending`]); the FIFO head is always the next observation to
//! solve, and solving it is the vehicle's single [`ActiveJob`]. Ordering per
//! vehicle is preserved because we never dispatch the second observation until
//! the first has committed and been popped.
//!
//! # The ready queue
//!
//! A vehicle is *ready* when it has a head, no active job, and is not
//! committing. Ready vehicles wait in a fair FIFO ([`Scheduler::next_ready`])
//! so no vehicle can starve another: a vehicle that finishes a job and still
//! has work rejoins the back of the queue behind everyone already waiting. The
//! queue holds each vehicle at most once (a side [`HashSet`] guards against
//! double insertion).
//!
//! # Coalescing and committed suppression
//!
//! JetStream can deliver the same raw message twice. If an observation whose
//! [`ObservationId`] is already pending or already the active job arrives
//! again, it is *coalesced* — dropped without a second FIFO slot — and its ack
//! handle handed straight back so the caller can acknowledge the duplicate.
//! Likewise, once a checkpoint is loaded, an observation at or below the last
//! committed input sequence is *already committed*: it is suppressed the same
//! way. Both keep redeliveries from ever re-solving finished work.
//!
//! # State diagram
//!
//! ```text
//!                    enqueue (first obs)
//!   (untracked) ─────────────────────────▶ Idle ──────────────┐ becomes READY
//!                                            │  head, no active │
//!                                            │  not committing  │
//!                                next_ready + activate          │
//!                                            ▼                   │
//!                                          Active ◀──────────────┘
//!                                   head kept, active=Some
//!                                    │              │
//!                             begin_commit        abandon (drop job, keep head)
//!                                    ▼              ▼
//!                               Committing        Idle ─▶ READY (re-dispatch head)
//!                             active=Some, flag
//!                                    │
//!                                  finish
//!                                    ▼
//!                       head popped, active taken, flag cleared
//!                          │                             │
//!                    pending empty                 pending non-empty
//!                          ▼                             ▼
//!                   Idle (evictable)              Idle (new head) ─▶ READY
//! ```
//!
//! ## Deadline expiry versus a real result
//!
//! When a job's deadline fires the worker commits a `Terminal` outcome through
//! the ordinary commit path, so the head is popped exactly as a real result
//! would pop it: [`Scheduler::begin_commit`] (the active job is still present),
//! then commit, then [`Scheduler::finish`]. The `committing` flag is the
//! arbitration point the spec demands — "a prepared commit always wins over a
//! later timeout". If a result's commit already set the flag, `begin_commit`
//! returns [`SchedulerError::AlreadyCommitting`] and the worker drops the
//! timeout. [`Scheduler::abandon`] is the *other* move: it drops the active job
//! (releasing its admission permit) **without** popping the head, so the same
//! observation can be re-dispatched — a retry, not a commit.
//!
//! # Parking early results
//!
//! A result can outrun the state that should consume it (a redelivery, or a
//! result for a job the worker has not finished wiring up). Such a result is
//! *parked* in a small bounded buffer ([`SchedulerConfig::parked_limit`]) and
//! replayed later via [`Scheduler::take_parked`]; parking past the bound drops
//! the result, which is safe because the matcher will re-derive it.
//!
//! # Eviction
//!
//! Idle vehicles are reclaimed by [`Scheduler::evict_idle`], which only removes
//! a vehicle with no pending work, no active job, and no commit in flight whose
//! `last_touch` has aged past [`SchedulerConfig::idle_ttl`]. In-flight state is
//! therefore never evicted. A vehicle in the ready queue always has a non-empty
//! pending queue (it was queued *because* it had a head, and its head cannot be
//! popped until it leaves the queue via `next_ready`), so eviction — which
//! requires an empty pending queue — never races the ready queue.

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
/// Loading a checkpoint is an async store round-trip the scheduler never makes
/// itself, so the state starts [`Unloaded`](CheckpointState::Unloaded) and the
/// worker fills it in with [`Scheduler::set_checkpoint`] once the store answers.
/// The distinction between [`Unloaded`](CheckpointState::Unloaded) and
/// [`Absent`](CheckpointState::Absent) matters: only a *loaded* checkpoint
/// lets [`Scheduler::enqueue`] suppress an already-committed observation, so an
/// unloaded vehicle never mistakes "not looked yet" for "nothing committed".
///
// `Present` dominates the enum's size, but boxing it would add an indirection
// on the common checkpoint read — a tracked vehicle almost always has a loaded
// checkpoint — and the shared contract fixes this shape (other orchestrator
// tasks match `Present(cp)` for a bare `VehicleCheckpoint`). The value lives
// one-per-vehicle inside an already heap-allocated map entry, so the inline
// size costs nothing extra.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum CheckpointState<E: Entry> {
    /// The store has not been consulted yet.
    Unloaded,
    /// The store was consulted and this vehicle has no committed checkpoint
    /// (a fresh vehicle).
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
/// The `handle` is the durable delivery: the observation is not acknowledged to
/// the broker until the work it drives has committed (or it is coalesced /
/// suppressed / overflowed, in which case the handle travels back out in the
/// [`Enqueue`] result so the caller can ack it immediately).
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

/// The single logical solve a vehicle has in flight.
///
/// It owns the [`admission::Permit`] reserved for the job, so "outstanding"
/// means exactly "a live `ActiveJob` (or a permit moved out of one) exists".
/// The permit is released when the job is dropped — on [`Scheduler::finish`] or
/// [`Scheduler::abandon`].
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
    /// The admission reservation held for the lifetime of the job.
    pub permit: admission::Permit,
    /// When the job was dispatched — used to measure solve latency.
    pub dispatched: Instant,
}

/// Everything a partition worker remembers about one vehicle.
///
/// The fields are public so the worker can read them directly on its hot path;
/// the scheduler owns the *transitions* between them and keeps the ready queue
/// consistent with them.
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

    /// Reclaimable: no pending work, no active job, not committing. Parked
    /// results do not keep an otherwise-idle vehicle alive — they are bounded
    /// and re-derivable — matching the spec's eviction predicate exactly.
    fn is_idle(&self) -> bool {
        self.pending.is_empty() && self.active.is_none() && !self.committing
    }
}

/// The outcome of offering an observation to the scheduler.
///
/// Three of the four variants hand the ack handle back inside the variant: the
/// observation is not going to occupy a FIFO slot, so the caller acknowledges
/// its raw delivery straight away rather than leaking it.
#[must_use = "an enqueue result may carry an ack handle that must be acknowledged"]
pub enum Enqueue<H: AckHandle> {
    /// The observation was appended to the vehicle's FIFO; `depth` is the new
    /// queue length including this observation.
    Queued {
        /// The vehicle's pending depth after the append.
        depth: usize,
    },
    /// A duplicate transport delivery of an observation already pending or
    /// active; nothing was enqueued. The handle is returned to be acked.
    Coalesced(H),
    /// The observation is at or below the last committed input, so it is
    /// already decided; nothing was enqueued. The handle is returned to be
    /// acked.
    Committed(H),
    /// The vehicle's FIFO is at [`SchedulerConfig::pending_limit`]; the
    /// observation was refused. The handle is returned so the caller can nak or
    /// drop it.
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
    /// The park buffer was full (or the vehicle is untracked); the result was
    /// dropped. `limit` is the configured bound.
    Dropped {
        /// The park-buffer bound that was hit.
        limit: usize,
    },
}

/// The head observation and active job handed back by [`Scheduler::finish`].
pub struct Finished<H: AckHandle> {
    /// The head observation just popped from the FIFO. Its ack handle retires
    /// the raw delivery once the caller has committed.
    pub observation: PendingObservation<H>,
    /// The active job whose result (or terminal outcome) was committed. Dropping
    /// it releases the admission permit.
    pub job: ActiveJob,
}

/// A point-in-time count of what the scheduler is holding. Bounded, label-free
/// numbers suitable for the partition worker's gauges.
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
    /// How long a vehicle may sit idle before [`Scheduler::evict_idle`] reclaims
    /// it.
    pub idle_ttl: Duration,
    /// Most observations a single vehicle may hold pending before further ones
    /// overflow.
    pub pending_limit: usize,
}

impl Default for SchedulerConfig {
    /// The spec defaults: 4 parked results, a 10-minute idle TTL, and 256
    /// pending observations per vehicle.
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
    /// [`Scheduler::begin_commit`]/[`Scheduler::finish`] on a vehicle with no
    /// active job.
    #[error("vehicle has no active job")]
    NoActiveJob,
    /// [`Scheduler::begin_commit`] on a vehicle that is already committing — the
    /// arbitration point where a prepared commit beats a later timeout.
    #[error("vehicle is already committing")]
    AlreadyCommitting,
    /// [`Scheduler::finish`] found no head to pop — an invariant violation
    /// (an active job always has its head in the FIFO).
    #[error("vehicle has no head observation to finish")]
    NoHead,
}

/// The per-partition vehicle table and ready queue.
///
/// Generic over the entry type `E` (the network's node id, carried through
/// checkpoints and results) and the ack-handle type `H` (the transport's
/// delivery receipt). A partition worker owns one of these and never shares it.
pub struct Scheduler<E: Entry, H: AckHandle> {
    config: SchedulerConfig,
    vehicles: HashMap<VehicleId, VehicleState<E, H>>,
    ready: VecDeque<VehicleId>,
    ready_set: HashSet<VehicleId>,
    /// A standing `Unloaded` returned by [`Scheduler::checkpoint`] for an
    /// untracked vehicle, so that accessor can be total without an `Option`.
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

    /// Offer an observation to its vehicle's FIFO.
    ///
    /// Creates the vehicle if it is new. Returns [`Enqueue::Committed`] if the
    /// vehicle's loaded checkpoint already covers this observation,
    /// [`Enqueue::Coalesced`] if the same observation id is already pending or
    /// active, [`Enqueue::Overflow`] if the FIFO is full, and otherwise
    /// [`Enqueue::Queued`] with the new depth. A vehicle that becomes eligible
    /// as a result is added to the ready queue.
    pub fn enqueue(&mut self, obs: PendingObservation<H>, now: Instant) -> Enqueue<H> {
        let vehicle = obs.payload.vehicle_id;
        let limit = self.config.pending_limit;
        let state = self
            .vehicles
            .entry(vehicle)
            .or_insert_with(|| VehicleState::new(now));
        state.last_touch = now;

        // Already committed: a redelivery of an observation at or below the last
        // committed input. Only decidable once a checkpoint is loaded.
        if let CheckpointState::Present(cp) = &state.checkpoint
            && obs.id.sequence <= cp.last_input.sequence
        {
            return Enqueue::Committed(obs.handle);
        }

        // Duplicate transport delivery: the same observation id is already
        // pending or is the active job. Coalesce it rather than queue a twin.
        if state
            .active
            .as_ref()
            .is_some_and(|a| a.observation == obs.id)
            || state.pending.iter().any(|p| p.id == obs.id)
        {
            return Enqueue::Coalesced(obs.handle);
        }

        // FIFO full.
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
    /// Ready-queue entries are re-checked for eligibility on the way out, so a
    /// stale entry (one whose vehicle stopped being eligible without leaving the
    /// queue) is skipped rather than returned.
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

    /// The vehicle's checkpoint state. Returns a standing `Unloaded` for an
    /// untracked vehicle so the accessor never needs an `Option`.
    #[must_use]
    pub fn checkpoint(&self, vehicle: VehicleId) -> &CheckpointState<E> {
        self.vehicles
            .get(&vehicle)
            .map_or(&self.absent_checkpoint, |s| &s.checkpoint)
    }

    /// Record a loaded checkpoint for a tracked vehicle. A no-op for an
    /// untracked vehicle — checkpoints are only loaded for vehicles that already
    /// have pending work.
    pub fn set_checkpoint(&mut self, vehicle: VehicleId, checkpoint: CheckpointState<E>) {
        if let Some(state) = self.vehicles.get_mut(&vehicle) {
            state.checkpoint = checkpoint;
        }
    }

    /// Attach the one logical job to a vehicle.
    ///
    /// The caller must have taken the vehicle from [`Scheduler::next_ready`], so
    /// it has a head and is not already active. Returns
    /// [`SchedulerError::AlreadyActive`] if a job is somehow already attached,
    /// or [`SchedulerError::UnknownVehicle`] if the vehicle is not tracked.
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

    /// Mark that a commit is in flight for the vehicle's active job.
    ///
    /// This is the arbitration flag: while it is set the vehicle is never
    /// evicted and a later deadline defers to the commit. Returns
    /// [`SchedulerError::NoActiveJob`] if there is nothing to commit, or
    /// [`SchedulerError::AlreadyCommitting`] if a commit is already in flight —
    /// the case where a prepared commit has already beaten a timeout.
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

    /// Complete the active job: pop the head observation and take the job, clear
    /// the committing flag, and re-ready the vehicle if it still has pending
    /// work.
    ///
    /// This is the terminal move of every committed outcome — a real match or a
    /// deadline `Terminal` alike — because both advance the vehicle past its
    /// head. Returns [`SchedulerError::NoActiveJob`] if no job is attached.
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

    /// Drop the active job **without** popping the head, returning the job so
    /// the caller can release its permit. The head stays, so the same
    /// observation can be re-dispatched; the vehicle is re-readied if it is now
    /// eligible.
    ///
    /// This is the retry path (a rejected or superseded result), distinct from
    /// [`Scheduler::finish`] which advances past the head. It expects the
    /// vehicle not to be committing; a committing vehicle is left un-readied.
    /// Returns `None` if the vehicle is untracked or has no active job.
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
    /// [`SchedulerConfig::parked_limit`]. Returns [`Park::Dropped`] if the
    /// buffer is full or the vehicle is untracked.
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

    /// Remove every vehicle that is idle — no pending, no active, not
    /// committing — and whose `last_touch` has aged past
    /// [`SchedulerConfig::idle_ttl`]. Returns the evicted ids.
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

    /// The age of the oldest head observation across all vehicles — the "oldest
    /// eligible queue age" the spec tracks — or `None` if nothing is pending.
    #[must_use]
    pub fn oldest_pending(&self, now: Instant) -> Option<Duration> {
        self.vehicles
            .values()
            .filter_map(|s| s.pending.front())
            .map(|p| p.received)
            .min()
            .map(|oldest| now.saturating_duration_since(oldest))
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

    /// A recorded acknowledgement, captured by [`TestAck`].
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum AckOp {
        Ack(u64),
        Nak(u64),
    }

    /// A tiny [`AckHandle`] that records its ack/nak into a shared log so a test
    /// can assert the right handle came back and was retired.
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

    /// Shared fixtures so each test reads as a state-machine story, not setup.
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
                permit: self.admission.try_admit(&self.region, 100).unwrap(),
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

    /// Enqueue a setup observation, asserting it was queued (fresh ids on fresh
    /// vehicles always are) and retiring the `must_use` result.
    #[track_caller]
    fn feed(
        sched: &mut Scheduler<MockEntryId, TestAck>,
        obs: PendingObservation<TestAck>,
        now: Instant,
    ) {
        assert!(matches!(sched.enqueue(obs, now), Enqueue::Queued { .. }));
    }

    /// Enqueue asserts a plain `Queued { depth }` and returns the depth.
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

        // The head is the earliest sequence; it stays put until finished.
        assert_eq!(sched.head(VehicleId(1)).unwrap().id.sequence, 10);

        // One vehicle is ready exactly once regardless of depth.
        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
        assert_eq!(sched.next_ready(), None);

        // Dispatch, finish, and the next head is the second observation.
        sched.activate(VehicleId(1), fx.job(1, 10, now)).unwrap();
        let finished = sched.finish(VehicleId(1), now).unwrap();
        assert_eq!(finished.observation.id.sequence, 10);
        assert_eq!(sched.head(VehicleId(1)).unwrap().id.sequence, 11);

        // Re-readied because work remains.
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

        // FIFO across vehicles in the order they first became ready.
        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
        assert_eq!(sched.next_ready(), Some(VehicleId(2)));
        assert_eq!(sched.next_ready(), Some(VehicleId(3)));
        assert_eq!(sched.next_ready(), None);

        // Vehicle 1 finishes and gets more work: it rejoins the back, so a
        // vehicle cannot monopolise the worker.
        sched.activate(VehicleId(1), fx.job(1, 1, now)).unwrap();
        feed(&mut sched, fx.obs(1, 2, now), now); // queued behind the active head
        feed(&mut sched, fx.obs(2, 2, now), now); // vehicle 2 re-ready first
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
        // Not enqueued twice.
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

        // A redelivery of the observation now being solved coalesces too.
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

        // Track the vehicle, then load a checkpoint whose last input is seq 20.
        feed(&mut sched, fx.obs(1, 21, now), now);
        sched.set_checkpoint(VehicleId(1), fx.present_checkpoint(20));

        // A redelivery at or below the committed input is suppressed...
        match sched.enqueue(fx.obs(1, 20, now), now) {
            Enqueue::Committed(handle) => assert_eq!(handle.sequence(), 20),
            _ => panic!("expected Committed at the boundary"),
        }
        match sched.enqueue(fx.obs(1, 19, now), now) {
            Enqueue::Committed(handle) => assert_eq!(handle.sequence(), 19),
            _ => panic!("expected Committed below the boundary"),
        }
        // ...but a newer observation is still queued.
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

        // With no checkpoint loaded, even a low sequence is queued: the
        // scheduler must not guess what has committed.
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

        // No active job yet.
        feed(&mut sched, fx.obs(1, 1, now), now);
        assert_eq!(
            sched.begin_commit(VehicleId(1)),
            Err(SchedulerError::NoActiveJob)
        );

        sched.next_ready();
        sched.activate(VehicleId(1), fx.job(1, 1, now)).unwrap();
        assert_eq!(sched.begin_commit(VehicleId(1)), Ok(()));
        // A prepared commit beats any second attempt (e.g. a later timeout).
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
        // Nothing left to do, so the vehicle is not ready.
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
        // Head kept; the same observation is eligible to re-dispatch.
        assert_eq!(sched.head(VehicleId(1)).unwrap().id.sequence, 1);
        assert_eq!(sched.next_ready(), Some(VehicleId(1)));
        // Nothing to abandon a second time.
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
        // The buffer is empty after draining, so parking resumes.
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

        // Vehicle 1: pending (not idle). Vehicle 2: will go active (not idle).
        // Vehicle 3: will be committing (not idle). Vehicle 4: idle.
        let t0 = fx.at(0);
        feed(&mut sched, fx.obs(1, 1, t0), t0);

        feed(&mut sched, fx.obs(2, 1, t0), t0);
        sched.next_ready(); // vehicle 1
        sched.next_ready(); // vehicle 2
        sched.activate(VehicleId(2), fx.job(2, 1, t0)).unwrap();

        feed(&mut sched, fx.obs(3, 1, t0), t0);
        sched.next_ready(); // vehicle 3
        sched.activate(VehicleId(3), fx.job(3, 1, t0)).unwrap();
        sched.begin_commit(VehicleId(3)).unwrap();

        // Vehicle 4 becomes idle: enqueue then finish to empty its FIFO.
        feed(&mut sched, fx.obs(4, 1, t0), t0);
        sched.next_ready(); // vehicle 4
        sched.activate(VehicleId(4), fx.job(4, 1, t0)).unwrap();
        sched.finish(VehicleId(4), t0).unwrap();

        // Before the TTL, nothing is evicted.
        assert!(sched.evict_idle(t0).is_empty());

        // After the TTL, only the idle vehicle 4 is reclaimed.
        let after = t0 + ttl;
        let evicted = sched.evict_idle(after);
        assert_eq!(evicted, vec![VehicleId(4)]);

        let stats = sched.stats();
        assert_eq!(stats.vehicles, 3);
        assert_eq!(stats.pending, 3); // heads of 1, 2, 3 remain
    }

    #[test]
    fn oldest_pending_picks_the_oldest_head() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);

        // Vehicle 1's head is the oldest (received at t=0); vehicle 2 at t=5.
        feed(&mut sched, fx.obs(1, 1, fx.at(0)), fx.at(0));
        feed(&mut sched, fx.obs(1, 2, fx.at(20)), fx.at(20)); // not the head
        feed(&mut sched, fx.obs(2, 1, fx.at(5)), fx.at(5));

        let now = fx.at(30);
        assert_eq!(sched.oldest_pending(now), Some(Duration::from_secs(30)));

        // With nothing pending, there is no age.
        let empty = Scheduler::<MockEntryId, TestAck>::new(SchedulerConfig::default());
        assert_eq!(empty.oldest_pending(now), None);
    }

    #[test]
    fn checkpoint_accessor_is_total() {
        let fx = Fixture::new();
        let mut sched = scheduler(&fx);
        let now = fx.at(0);

        // Untracked vehicle reads as Unloaded.
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

        // Setting a checkpoint on an untracked vehicle is a no-op.
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
        // The caller acks the coalesced duplicate; the log records it.
        futures::executor::block_on(handle.ack()).unwrap();
        assert_eq!(&*fx.log.lock().unwrap(), &[AckOp::Ack(5)]);
    }
}
