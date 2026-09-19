//! The deadline and terminal-outcome handler.
//!
//! Stops a vehicle waiting forever on a job a matcher never answers. Two pure
//! pieces the worker drives, doing no I/O and reading no clock: [`Deadlines`], a
//! min-heap armed per dispatch and drained with [`Deadlines::pop_expired`], and
//! [`on_expiry`] / [`on_exhausted`], which classify a fired timer. Entries are
//! never removed eagerly; a stale one classifies as [`Expiry::Ignore`]. A
//! prepared commit always wins over a later timeout.

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

/// One armed deadline, ordered by fire instant first so a min-heap surfaces the
/// soonest; the ids only break ties for the total order [`BinaryHeap`] needs.
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

/// A partition worker's min-heap of pending job deadlines, keyed on fire instant.
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

    /// Arm a deadline: `job` on `vehicle` fires at `at`. May be armed more than
    /// once; the first to pop resolves it, the rest are [`Expiry::Ignore`].
    pub fn arm(&mut self, at: Instant, vehicle: VehicleId, job: JobId) {
        self.heap.push(Reverse(Timer { at, vehicle, job }));
    }

    /// The soonest fire instant, or `None` when nothing is armed. May point at a
    /// job that already committed, causing one harmless early wake.
    #[must_use]
    pub fn next_at(&self) -> Option<Instant> {
        self.heap.peek().map(|Reverse(timer)| timer.at)
    }

    /// Drain every deadline due at or before `now`, soonest-first. The pairs are
    /// candidates: each must still be run through [`on_expiry`]/[`on_exhausted`].
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
    /// The timer no longer matters (stale, or already committing); dropped.
    Ignore,
    /// The live, uncommitted job: commit a terminal outcome.
    Terminal(TerminalReason),
}

/// Classify a fired deadline for `job` against a vehicle's `state`, resolving to
/// [`Expiry::Terminal`] only when `job` is live and uncommitted.
#[must_use]
pub fn on_expiry<E, H>(state: &VehicleState<E, H>, job: JobId) -> Expiry
where
    E: Entry,
    H: AckHandle,
{
    arbitrate(state, job, TerminalReason::DeadlineExpired)
}

/// Like [`on_expiry`] but for a "job exhausted" signal, recorded as
/// [`TerminalReason::JobExhausted`].
#[must_use]
pub fn on_exhausted<E, H>(state: &VehicleState<E, H>, job: JobId) -> Expiry
where
    E: Entry,
    H: AckHandle,
{
    arbitrate(state, job, TerminalReason::JobExhausted)
}

/// The shared arbitration: `job` must be the live active job and the vehicle
/// must not already be committing, else the signal is ignored.
fn arbitrate<E, H>(state: &VehicleState<E, H>, job: JobId, reason: TerminalReason) -> Expiry
where
    E: Entry,
    H: AckHandle,
{
    match &state.active {
        Some(active) if active.id != job => Expiry::Ignore,
        // The live job, but a prepared commit already wins over the timeout.
        Some(_) if state.committing => Expiry::Ignore,
        Some(_) => Expiry::Terminal(reason),
        None => Expiry::Ignore,
    }
}

/// Build the terminal [`commit::Decision`] for `job`. `closes_segment` is always
/// `false`: a deadline ends the job, not the vehicle's continuity.
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
    /// Extra time added to a job's deadline before expiry, to absorb jitter;
    /// `Duration::ZERO` (the default) uses the deadline exactly.
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
    /// The instant a job with deadline `job_deadline` should fire, including
    /// [`grace`](DeadlineConfig::grace).
    #[must_use]
    pub fn fire_at(&self, job_deadline: Instant) -> Instant {
        job_deadline + self.grace
    }
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;

    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::{Continuation, Trip};

    use super::*;
    use crate::event::VehicleId;
    use crate::orchestrator::admission::{Admission, AdmissionConfig};
    use crate::orchestrator::commit;
    use crate::orchestrator::scheduler::{CheckpointState, JobReservation};
    use crate::orchestrator::validate::{self, RejectReason, Verdict};
    use crate::partition::partition_of;
    use crate::protocol::ids::{
        GraphVersion, Lane, ObservationId, RegionId, Revision, SCHEMA_VERSION, SegmentId,
    };
    use crate::protocol::job::{BaseState, JobIdentity, SolveJob};
    use crate::protocol::result::{SolveOutcome, SolveResult};
    use crate::store::checkpoint::VehicleCheckpoint;

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
            let solve_job = self.solve_job(vehicle, seq, base);
            ActiveJob {
                id: solve_job.id,
                identity: solve_job.identity,
                observation: self.obs(seq),
                deadline: self.base + Duration::from_secs(30),
                bytes: 100,
                reservation: JobReservation::Admitted(
                    self.admission.try_admit(&self.region, 100).unwrap(),
                ),
                dispatched: self.base,
            }
        }

        fn result(
            &self,
            vehicle: u64,
            seq: u64,
            base: Option<BaseState>,
        ) -> SolveResult<MockEntryId> {
            let job = self.solve_job(vehicle, seq, base);
            SolveResult::new(&job, SolveOutcome::Unanchored, 0)
        }

        fn solve_job(
            &self,
            vehicle: u64,
            seq: u64,
            base: Option<BaseState>,
        ) -> SolveJob<MockEntryId> {
            SolveJob::new(
                self.identity(vehicle, seq, base),
                Lane::DEFAULT,
                1_000,
                Continuation::Restart { fresh: Vec::new() },
            )
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
        deadlines.arm(fx.base + Duration::from_secs(3), VehicleId(3), JobId(30));
        deadlines.arm(fx.base + Duration::from_secs(1), VehicleId(1), JobId(10));
        deadlines.arm(fx.base + Duration::from_secs(2), VehicleId(2), JobId(20));

        assert_eq!(deadlines.len(), 3);
        assert_eq!(deadlines.next_at(), Some(fx.base + Duration::from_secs(1)));

        let due = deadlines.pop_expired(fx.base + Duration::from_secs(2));
        assert_eq!(
            due,
            vec![(VehicleId(1), JobId(10)), (VehicleId(2), JobId(20))]
        );
        assert_eq!(deadlines.next_at(), Some(fx.base + Duration::from_secs(3)));

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
        let cfg = DeadlineConfig::default();
        assert_eq!(
            cfg.fire_at(fx.base + Duration::from_secs(5)),
            fx.base + Duration::from_secs(5)
        );
        let cfg = DeadlineConfig {
            grace: Duration::from_secs(2),
        };
        assert_eq!(
            cfg.fire_at(fx.base + Duration::from_secs(5)),
            fx.base + Duration::from_secs(7)
        );
    }

    #[test]
    fn late_result_after_terminal_is_rejected() {
        let fx = Fixture::new();
        let segment = SegmentId(5);

        let job = fx.job(1, 10, Some(base(9, 5)));
        let job_id = job.id;
        let prev = fx.checkpoint(9, 5);

        let vehicle = fx.vehicle(
            CheckpointState::Present(fx.checkpoint(9, 5)),
            Some(job),
            false,
        );
        assert_eq!(
            on_expiry(&vehicle, job_id),
            Expiry::Terminal(TerminalReason::DeadlineExpired)
        );

        let active = vehicle.active.as_ref().expect("live job present");
        let decision = terminal_decision(active, TerminalReason::DeadlineExpired, segment);
        let plan = commit::plan(Some(&prev), decision, fx.obs(10), &fx.region, &fx.graph, 0);
        assert_eq!(plan.next.last_input, fx.obs(10));

        let after = fx.vehicle(CheckpointState::Present(plan.next), None, false);
        let late = fx.result(1, 10, Some(base(9, 5)));

        assert!(matches!(
            validate::validate(&after, &late, part(1)),
            Verdict::Reject(RejectReason::ExpiredJob)
        ));
    }
}
