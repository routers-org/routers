//! Orchestrator: the deadline and terminal-outcome handler (spec §3, §8).
//!
//! Every dispatched job carries a deadline. If a matcher never answers — it
//! crashed, the result was lost, or the job simply ran to exhaustion without an
//! emission — the vehicle would otherwise wait forever, its head observation
//! never popped and every later observation for that vehicle stuck behind it.
//! This module is the timer that stops that: the worker arms a deadline when it
//! dispatches, and when the timer fires the worker asks here what to do.
//!
//! Nothing in this file does I/O or reads the clock on its own behalf. It is two
//! pure pieces the worker drives:
//!
//! * [`Deadlines`] — a min-heap of `(fire-instant, vehicle, job)` the worker
//!   uses to know *when* to wake ([`Deadlines::next_at`]) and *which* jobs are
//!   due ([`Deadlines::pop_expired`]).
//! * [`on_expiry`] / [`on_exhausted`] — the arbitration that decides whether a
//!   fired timer still matters, and [`terminal_decision`], which builds the
//!   [`commit::Decision`] the worker hands to [`commit::plan`].
//!
//! # Why entries are never removed eagerly
//!
//! A job usually finishes with a real result long before its deadline. Rather
//! than hunt the heap for that job's entry and delete it — an O(n) scan on every
//! commit — the entry is left in place and becomes *stale*: when it eventually
//! pops, the vehicle either has no active job, has moved on to a different job,
//! or is already committing. [`on_expiry`] recognises every one of those as
//! [`Expiry::Ignore`], so a stale pop costs one cheap classification and nothing
//! else. The heap therefore only ever grows by one per dispatch and shrinks by
//! whatever `pop_expired` drains; it never needs random-access removal.
//!
//! # The arbitration a timeout must lose
//!
//! The deadline path and the result path are two ways to resolve the *same*
//! per-vehicle transition, and the spec fixes the tie-breaker: "a prepared
//! commit always wins over a later timeout". That single rule lives in the
//! [`VehicleState::committing`] flag. If a result arrived first and its commit
//! is in flight, `committing` is set and a fired timer resolves to
//! [`Expiry::Ignore`] — the worker drops it. Only when no commit has been staged
//! does a timer become a [`TerminalReason::DeadlineExpired`] commit of its own.
//! Once *that* commit lands, the checkpoint's `last_input` has advanced past the
//! observation, so any real result that finally straggles in is rejected by the
//! result validator ([`validate`](crate::orchestrator::validate)) as already
//! decided — the `late_result_after_terminal_is_rejected` test below pins that
//! whole sequence end to end.

use alloc::collections::BinaryHeap;
use core::cmp::Ordering;
use core::cmp::Reverse;
use core::time::Duration;

use routers_network::Entry;
use tokio::time::Instant;

use crate::bus::adapter::AckHandle;
use crate::event::VehicleId;
use crate::orchestrator::commit::{self, Decision};
use crate::orchestrator::scheduler::{ActiveJob, VehicleState};
use crate::protocol::ids::{JobId, SegmentId};
use crate::protocol::output::TerminalReason;

/// One armed deadline: the instant it fires and the `(vehicle, job)` it belongs
/// to.
///
/// Ordered by fire instant first so a min-heap surfaces the soonest deadline;
/// the vehicle and job ids only break ties, giving a total order (required by
/// [`BinaryHeap`]) without ever affecting *when* a timer is considered due.
/// A dedicated type is needed because [`VehicleId`] and [`JobId`] do not derive
/// `Ord` (they are opaque hashes, not ordered quantities), so the tuple sketched
/// in the task draft cannot itself be a heap key — see the module report's
/// deviation note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Timer {
    at: Instant,
    vehicle: VehicleId,
    job: JobId,
}

impl Ord for Timer {
    fn cmp(&self, other: &Self) -> Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.vehicle.0.cmp(&other.vehicle.0))
            .then_with(|| self.job.0.cmp(&other.job.0))
    }
}

impl PartialOrd for Timer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A partition worker's set of pending job deadlines, kept as a min-heap keyed
/// on fire instant.
///
/// The worker owns exactly one of these. It [`arm`](Deadlines::arm)s a deadline
/// per dispatch, sleeps until [`next_at`](Deadlines::next_at), then drains the
/// due timers with [`pop_expired`](Deadlines::pop_expired) and classifies each
/// through [`on_expiry`]. Stale entries (jobs that finished normally) are left in
/// the heap until they pop and are then ignored, so no eager removal is needed.
#[derive(Debug, Default)]
pub struct Deadlines {
    heap: BinaryHeap<Reverse<Timer>>,
}

impl Deadlines {
    /// A new, empty deadline set.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether any deadline is armed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.heap.is_empty()
    }

    /// How many deadlines are armed, including entries now stale.
    #[must_use]
    pub fn len(&self) -> usize {
        self.heap.len()
    }

    /// Arm a deadline: `job` on `vehicle` fires at `at`.
    ///
    /// The same `(vehicle, job)` may be armed more than once (a re-dispatch, or a
    /// grace re-arm); the extra entries are harmless — the first to pop resolves
    /// the job and the rest classify as [`Expiry::Ignore`].
    pub fn arm(&mut self, at: Instant, vehicle: VehicleId, job: JobId) {
        self.heap.push(Reverse(Timer { at, vehicle, job }));
    }

    /// The soonest fire instant, or `None` when nothing is armed.
    ///
    /// The worker feeds this to `tokio::time::sleep_until` so it wakes exactly
    /// when the next deadline is due and no sooner. Because entries are never
    /// removed eagerly, this may point at a job that has already committed; that
    /// only causes one early wake whose drained timer classifies as
    /// [`Expiry::Ignore`].
    #[must_use]
    pub fn next_at(&self) -> Option<Instant> {
        self.heap.peek().map(|Reverse(timer)| timer.at)
    }

    /// Drain and return every deadline whose fire instant is at or before `now`.
    ///
    /// The returned `(vehicle, job)` pairs are *candidates*: each still has to be
    /// run through [`on_expiry`] (or [`on_exhausted`]) against the vehicle's
    /// current state, because a job may have finished normally since it was armed
    /// and left a stale entry behind. Order is soonest-first.
    #[must_use]
    pub fn pop_expired(&mut self, now: Instant) -> Vec<(VehicleId, JobId)> {
        let mut due = Vec::new();
        while let Some(Reverse(timer)) = self.heap.peek() {
            if timer.at > now {
                break;
            }
            let Reverse(timer) = self.heap.pop().expect("peeked entry is present");
            due.push((timer.vehicle, timer.job));
        }
        due
    }
}

/// What a fired (or exhausted) deadline means for the vehicle it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Expiry {
    /// The timer no longer matters: the job is not the one in flight (it
    /// finished and its entry is stale), or a commit is already being prepared
    /// for it (the prepared commit wins). The worker drops it.
    Ignore,
    /// The job is still the live, uncommitted one: commit a terminal outcome for
    /// it with this reason via [`terminal_decision`].
    Terminal(TerminalReason),
}

/// Classify a fired deadline for `job` against a vehicle's current `state`.
///
/// Resolves to [`Expiry::Terminal`] with [`TerminalReason::DeadlineExpired`]
/// only when `job` is exactly the vehicle's active job *and* no commit is in
/// flight for it. Every other case — a different active job, no active job, or a
/// commit already being prepared — is [`Expiry::Ignore`], which is what makes a
/// stale pop cheap and enforces "a prepared commit always wins over a later
/// timeout".
#[must_use]
pub fn on_expiry<E, H>(state: &VehicleState<E, H>, job: JobId) -> Expiry
where
    E: Entry,
    H: AckHandle,
{
    arbitrate(state, job, TerminalReason::DeadlineExpired)
}

/// Classify a "job exhausted" signal for `job` against a vehicle's current
/// `state`.
///
/// The wiring raises this when the broker gives up on a job (a JetStream
/// `MaxDeliveries` advisory, or a consumer `max_deliver` ceiling surfaced by the
/// pull loop): the job will never be answered, so it is treated exactly like an
/// expired deadline but recorded as [`TerminalReason::JobExhausted`] so the
/// terminal output names the real cause. The same arbitration applies — a
/// prepared commit still wins.
#[must_use]
pub fn on_exhausted<E, H>(state: &VehicleState<E, H>, job: JobId) -> Expiry
where
    E: Entry,
    H: AckHandle,
{
    arbitrate(state, job, TerminalReason::JobExhausted)
}

/// The shared arbitration behind [`on_expiry`] and [`on_exhausted`]: `job` must
/// be the live active job and the vehicle must not already be committing, else
/// the signal is ignored.
fn arbitrate<E, H>(state: &VehicleState<E, H>, job: JobId, reason: TerminalReason) -> Expiry
where
    E: Entry,
    H: AckHandle,
{
    match &state.active {
        // A different job is in flight (or the previous one finished and this
        // entry is stale): the signal does not name the current transition.
        Some(active) if active.id != job => Expiry::Ignore,
        // The live job — but a commit is already being prepared for it, so the
        // prepared commit wins and the timeout is dropped.
        Some(_) if state.committing => Expiry::Ignore,
        // The live, uncommitted job: this signal resolves the transition.
        Some(_) => Expiry::Terminal(reason),
        // No job in flight at all: nothing to terminate.
        None => Expiry::Ignore,
    }
}

/// Build the [`commit::Decision`] a worker hands to [`commit::plan`] to commit a
/// terminal outcome for `job`.
///
/// `closes_segment` is always `false`: a deadline or exhaustion ends the *job*,
/// not the vehicle's continuity — the retained trip and finality watermark are
/// preserved so the next observation resumes the same segment. (A segment is
/// only closed deliberately, by a reset.) `segment` is the vehicle's current
/// segment, which the worker already holds.
#[must_use]
pub fn terminal_decision<E>(
    job: &ActiveJob,
    reason: TerminalReason,
    segment: SegmentId,
) -> Decision<E>
where
    E: Entry,
{
    commit::Decision::Terminal {
        job: job.id,
        identity: job.identity.clone(),
        reason,
        closes_segment: false,
        segment,
    }
}

/// Tuning for the deadline handler.
#[derive(Clone, Copy, Debug)]
pub struct DeadlineConfig {
    /// Extra time added to a job's own deadline before the worker treats it as
    /// expired. `Duration::ZERO` (the default) means the job's `deadline_us` is
    /// the deadline exactly; the knob exists so an operator can absorb a little
    /// broker or clock jitter without prematurely terminating slow-but-live
    /// jobs.
    pub grace: Duration,
}

impl Default for DeadlineConfig {
    fn default() -> Self {
        Self {
            grace: Duration::ZERO,
        }
    }
}

impl DeadlineConfig {
    /// The instant a job whose own deadline is `job_deadline` should fire,
    /// accounting for [`grace`](DeadlineConfig::grace). The worker passes the
    /// result to [`Deadlines::arm`].
    #[must_use]
    pub fn fire_at(&self, job_deadline: Instant) -> Instant {
        job_deadline + self.grace
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;

    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::Trip;

    use super::*;
    use crate::event::VehicleId;
    use crate::orchestrator::admission::{Admission, AdmissionConfig};
    use crate::orchestrator::commit;
    use crate::orchestrator::scheduler::CheckpointState;
    use crate::orchestrator::validate::{self, RejectReason, Verdict};
    use crate::partition::partition_of;
    use crate::protocol::ids::{
        GraphVersion, ObservationId, RegionId, Revision, SCHEMA_VERSION, SegmentId,
    };
    use crate::protocol::job::{BaseState, JobIdentity};
    use crate::protocol::result::{SolveOutcome, SolveResult};
    use crate::store::checkpoint::VehicleCheckpoint;

    /// A minimal ack handle; the deadline handler never acks, so the vehicle's
    /// handle type only has to exist.
    #[derive(Clone)]
    struct TestAck;

    impl AckHandle for TestAck {
        async fn ack(self) -> anyhow::Result<()> {
            Ok(())
        }
        async fn nak(self, _delay: Option<Duration>) -> anyhow::Result<()> {
            Ok(())
        }
        fn sequence(&self) -> u64 {
            0
        }
        fn deliveries(&self) -> u32 {
            1
        }
    }

    /// Shared setup: an admission controller (the only source of real permits an
    /// [`ActiveJob`] needs), a region, a graph and a monotonic base instant.
    struct Fixture {
        admission: Admission,
        region: RegionId,
        graph: GraphVersion,
        base: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            let region = RegionId::new("r1").unwrap();
            let admission = Admission::new(AdmissionConfig::default(), core::iter::once(&region));
            Self {
                admission,
                region,
                graph: GraphVersion::new("g1").unwrap(),
                base: Instant::now(),
            }
        }

        fn obs(&self, seq: u64) -> ObservationId {
            ObservationId {
                partition: 7,
                sequence: seq,
            }
        }

        fn identity(&self, vehicle: u64, seq: u64, base: Option<BaseState>) -> JobIdentity {
            JobIdentity {
                schema: SCHEMA_VERSION,
                vehicle_id: VehicleId(vehicle),
                observation: self.obs(seq),
                base,
                graph: self.graph.clone(),
                region: self.region.clone(),
            }
        }

        fn job(&self, vehicle: u64, seq: u64, base: Option<BaseState>) -> ActiveJob {
            let identity = self.identity(vehicle, seq, base);
            ActiveJob {
                id: identity.job_id(),
                identity,
                observation: self.obs(seq),
                deadline: self.base + Duration::from_secs(30),
                bytes: 100,
                permit: self.admission.try_admit(&self.region, 100).unwrap(),
                dispatched: self.base,
            }
        }

        fn result(
            &self,
            vehicle: u64,
            seq: u64,
            base: Option<BaseState>,
        ) -> SolveResult<MockEntryId> {
            let identity = self.identity(vehicle, seq, base);
            SolveResult {
                job: identity.job_id(),
                identity,
                outcome: SolveOutcome::Unanchored,
                solved_at_us: 0,
            }
        }

        fn checkpoint(&self, last_seq: u64, segment: u64) -> VehicleCheckpoint<MockEntryId> {
            VehicleCheckpoint {
                trip: Trip::new(),
                last_input: self.obs(last_seq),
                revision: Revision(last_seq),
                segment: SegmentId(segment),
                finalized_through: None,
                graph: self.graph.clone(),
                schema: SCHEMA_VERSION,
                region: self.region.clone(),
            }
        }

        fn vehicle(
            &self,
            checkpoint: CheckpointState<MockEntryId>,
            active: Option<ActiveJob>,
            committing: bool,
        ) -> VehicleState<MockEntryId, TestAck> {
            VehicleState {
                checkpoint,
                pending: VecDeque::new(),
                active,
                committing,
                parked: Vec::new(),
                last_touch: self.base,
            }
        }
    }

    fn part(vehicle: u64) -> u16 {
        partition_of(VehicleId(vehicle)) as u16
    }

    fn base(revision: u64, segment: u64) -> BaseState {
        BaseState {
            revision: Revision(revision),
            segment: SegmentId(segment),
        }
    }

    #[test]
    fn pop_expired_returns_due_timers_soonest_first() {
        let fx = Fixture::new();
        let mut deadlines = Deadlines::new();
        // Arm out of order; the heap must surface them by instant.
        deadlines.arm(fx.base + Duration::from_secs(3), VehicleId(3), JobId(30));
        deadlines.arm(fx.base + Duration::from_secs(1), VehicleId(1), JobId(10));
        deadlines.arm(fx.base + Duration::from_secs(2), VehicleId(2), JobId(20));

        assert_eq!(deadlines.len(), 3);
        assert_eq!(deadlines.next_at(), Some(fx.base + Duration::from_secs(1)));

        // At t+2 the first two are due, ordered soonest-first; the third is not.
        let due = deadlines.pop_expired(fx.base + Duration::from_secs(2));
        assert_eq!(
            due,
            vec![(VehicleId(1), JobId(10)), (VehicleId(2), JobId(20))]
        );
        assert_eq!(deadlines.next_at(), Some(fx.base + Duration::from_secs(3)));

        // The last drains only once its instant is reached.
        assert!(
            deadlines
                .pop_expired(fx.base + Duration::from_secs(2))
                .is_empty()
        );
        let rest = deadlines.pop_expired(fx.base + Duration::from_secs(5));
        assert_eq!(rest, vec![(VehicleId(3), JobId(30))]);
        assert!(deadlines.is_empty());
        assert_eq!(deadlines.next_at(), None);
    }

    #[test]
    fn a_stale_pop_is_ignored() {
        // The armed job finished and the vehicle moved on to a different job.
        let fx = Fixture::new();
        let stale = JobId(999);
        let live = fx.job(1, 10, None);
        assert_ne!(live.id, stale);
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(live), false);

        assert_eq!(on_expiry(&vehicle, stale), Expiry::Ignore);
    }

    #[test]
    fn a_pop_with_no_active_job_is_ignored() {
        let fx = Fixture::new();
        let vehicle = fx.vehicle(CheckpointState::Unloaded, None, false);
        assert_eq!(on_expiry(&vehicle, JobId(1)), Expiry::Ignore);
    }

    #[test]
    fn a_prepared_commit_wins_over_a_later_timeout() {
        // The live job is being committed (committing = true): the timer loses.
        let fx = Fixture::new();
        let job = fx.job(1, 10, None);
        let id = job.id;
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(job), true);

        assert_eq!(on_expiry(&vehicle, id), Expiry::Ignore);
        assert_eq!(on_exhausted(&vehicle, id), Expiry::Ignore);
    }

    #[test]
    fn a_live_uncommitted_job_expires_terminal() {
        let fx = Fixture::new();
        let job = fx.job(1, 10, None);
        let id = job.id;
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(job), false);

        assert_eq!(
            on_expiry(&vehicle, id),
            Expiry::Terminal(TerminalReason::DeadlineExpired)
        );
    }

    #[test]
    fn exhaustion_of_a_live_job_is_terminal_with_its_own_reason() {
        let fx = Fixture::new();
        let job = fx.job(1, 10, None);
        let id = job.id;
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(job), false);

        assert_eq!(
            on_exhausted(&vehicle, id),
            Expiry::Terminal(TerminalReason::JobExhausted)
        );
    }

    #[test]
    fn terminal_decision_never_closes_the_segment() {
        let fx = Fixture::new();
        let job = fx.job(1, 10, Some(base(5, 3)));
        let decision: commit::Decision<MockEntryId> =
            terminal_decision(&job, TerminalReason::DeadlineExpired, SegmentId(3));

        match decision {
            commit::Decision::Terminal {
                job: job_id,
                reason,
                closes_segment,
                segment,
                identity,
            } => {
                assert_eq!(job_id, job.id);
                assert_eq!(reason, TerminalReason::DeadlineExpired);
                assert!(!closes_segment);
                assert_eq!(segment, SegmentId(3));
                assert_eq!(identity.observation, fx.obs(10));
            }
            other => panic!("expected Terminal, got {other:?}"),
        }
    }

    #[test]
    fn default_config_has_zero_grace_and_fire_at_respects_it() {
        let fx = Fixture::new();
        assert_eq!(DeadlineConfig::default().grace, Duration::ZERO);
        // Zero grace: the deadline is unchanged.
        let cfg = DeadlineConfig::default();
        assert_eq!(
            cfg.fire_at(fx.base + Duration::from_secs(5)),
            fx.base + Duration::from_secs(5)
        );
        // A non-zero knob shifts the fire instant out by the grace.
        let cfg = DeadlineConfig {
            grace: Duration::from_secs(2),
        };
        assert_eq!(
            cfg.fire_at(fx.base + Duration::from_secs(5)),
            fx.base + Duration::from_secs(7)
        );
    }

    /// The arbitration pin: once a deadline commits a `Terminal` for an
    /// observation, the checkpoint's `last_input` advances to it, so a real
    /// result that finally straggles in for that same observation is rejected by
    /// the result validator as already decided. This exercises the whole chain —
    /// [`terminal_decision`] -> [`commit::plan`] -> the new checkpoint ->
    /// [`validate::validate`] — rather than asserting it in prose.
    #[test]
    fn late_result_after_terminal_is_rejected() {
        let fx = Fixture::new();
        let segment = SegmentId(5);

        // A live job on observation 10, resuming from segment 5.
        let job = fx.job(1, 10, Some(base(9, 5)));
        let job_id = job.id;
        let prev = fx.checkpoint(9, 5);

        // The deadline fires with no commit in flight: terminal. Borrow the
        // active job back out of the vehicle (an `ActiveJob` holds an admission
        // permit and is deliberately not `Clone`).
        let vehicle = fx.vehicle(
            CheckpointState::Present(fx.checkpoint(9, 5)),
            Some(job),
            false,
        );
        assert_eq!(
            on_expiry(&vehicle, job_id),
            Expiry::Terminal(TerminalReason::DeadlineExpired)
        );

        // The worker commits that terminal via the ordinary commit path.
        let active = vehicle.active.as_ref().expect("live job present");
        let decision = terminal_decision(active, TerminalReason::DeadlineExpired, segment);
        let plan = commit::plan(Some(&prev), decision, fx.obs(10), &fx.region, &fx.graph, 0);
        // The installed checkpoint has advanced its committed input to obs 10.
        assert_eq!(plan.next.last_input, fx.obs(10));

        // The job is done: no active job, checkpoint installed. Now the real
        // result for observation 10 finally arrives.
        let after = fx.vehicle(CheckpointState::Present(plan.next), None, false);
        let late = fx.result(1, 10, Some(base(9, 5)));

        // It is rejected as already resolved — the terminal won the arbitration.
        assert!(matches!(
            validate::validate(&after, &late, part(1)),
            Verdict::Reject(RejectReason::ExpiredJob)
        ));
    }
}
