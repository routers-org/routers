//! Orchestrator: the pure result reader and validator (spec §3, §8).
//!
//! A partition worker reads solve results off its result plane and must decide,
//! for each one, whether it is *the* answer to the vehicle's in-flight job, an
//! answer that has arrived before the state that should consume it, an
//! answer that is definitively obsolete, or a poisoned duplicate that must be
//! kept out of the commit path. That decision is entirely a function of the
//! vehicle's current state and the result envelope — no I/O, no clock — so it
//! lives here as a pure function and the worker performs the side effects it
//! implies (commit, park, ack-and-drop, quarantine-and-ack).
//!
//! # Why a result can outrun its state
//!
//! Results are addressed to the vehicle's partition and read by the one owner
//! of that partition, but the owner rebuilds its per-vehicle state lazily: on
//! recovery it replays the raw journal, and a result for a job it dispatched
//! *before* a crash can be redelivered before replay has re-created the pending
//! observation and re-dispatched that job. Such a result is not obsolete — it
//! is early. The validator distinguishes "early" (park and revisit) from
//! "obsolete" (older than or equal to the committed input, a closed segment, a
//! stale base, or a foreign schema) so the worker never drops an answer it will
//! still need, and never commits one it must not.
//!
//! # Why quarantine exists
//!
//! Determinism means the same job identity always hashes to the same [`JobId`],
//! so a redelivered result for a job the worker has already committed carries
//! the *same* id. That is harmless only if the bytes are identical, which this
//! layer cannot check (it does not hold the prepared bytes). Rather than risk
//! committing a second, possibly-divergent answer over a finalized one, a
//! same-id result arriving once a commit is already in flight is quarantined —
//! never committed, acknowledged so the broker stops redelivering it, and kept
//! for diagnosis. Content comparison against the prepared bytes is the worker's
//! job (it may then raise [`QuarantineReason::ConflictingContent`]); this pure
//! function only sees identities.

use core::cmp::Ordering;

use routers_network::Entry;

use crate::bus::adapter::AckHandle;
use crate::event::MatchedDiff;
use crate::orchestrator::scheduler::{ActiveJob, VehicleState};
use crate::protocol::ids::SCHEMA_VERSION;
use crate::protocol::job::BaseState;
use crate::protocol::result::SolveResult;
use crate::store::checkpoint::VehicleCheckpoint;

/// The validator's decision for one solve result against one vehicle's state.
///
/// The lifetime `'a` borrows the vehicle's [`ActiveJob`] into
/// [`Verdict::Accept`] so the worker commits the very job it validated without
/// a second lookup. It is deliberately *not* generic over the network entry
/// type `E`: every variant is either entry-free or borrows the non-generic
/// [`ActiveJob`], so an `E` parameter would be unused (see the module report's
/// deviation note).
#[derive(Debug)]
pub enum Verdict<'a> {
    /// The expected result for the vehicle's active job: commit it. Borrows the
    /// active job so the worker commits exactly what was validated.
    Accept {
        /// The in-flight job this result answers.
        job: &'a ActiveJob,
    },
    /// No active job yet for this observation, but it is newer than the last
    /// committed input: replay has not reached it. Park (bounded) and revisit
    /// when the job is dispatched.
    Park,
    /// Definitively obsolete: at or below the committed input, from a closed
    /// segment, a stale base, or a schema the vehicle no longer uses. Ack and
    /// drop.
    Reject(RejectReason),
    /// A same-identity result that must never reach the commit path: a second
    /// answer for a job already being committed. Keep it for diagnosis.
    Quarantine(QuarantineReason),
}

impl Verdict<'_> {
    /// A bounded, low-cardinality label for the verdict class, suitable as a
    /// metric dimension. The finer reason lives on the inner enum's own
    /// [`RejectReason::label`] / [`QuarantineReason::label`].
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            Verdict::Accept { .. } => "accept",
            Verdict::Park => "park",
            Verdict::Reject(_) => "reject",
            Verdict::Quarantine(_) => "quarantine",
        }
    }
}

/// Why a result is definitively obsolete and may be acknowledged and dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RejectReason {
    /// The result's observation is at or below the vehicle's last committed
    /// input: its decision has already been made and superseded.
    Committed,
    /// The result resumes from a base the vehicle no longer holds — a different
    /// revision/segment than the one currently in flight or committed.
    StaleBase {
        /// The base the vehicle expects (its in-flight job's base), if any.
        expected: Option<BaseState>,
        /// The base the result was solved against, if any.
        got: Option<BaseState>,
    },
    /// The result belongs to a continuity segment the vehicle has since closed
    /// (a reset opened a new one); its layers must not be committed.
    SegmentClosed,
    /// The result's vehicle does not map to the partition this worker owns — it
    /// was addressed to the wrong reader.
    WrongVehicle,
    /// The result was produced against a wire schema this build does not serve.
    Schema,
    /// The echoed job id does not match the id its identity hashes to: a
    /// corrupted or crossed envelope.
    IdMismatch,
    /// The job for this exact observation has already been resolved and
    /// committed (for example by the deadline handler firing a `Terminal`
    /// before the real answer arrived).
    ExpiredJob,
}

impl RejectReason {
    /// A bounded, low-cardinality metric label; one stable string per variant.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            RejectReason::Committed => "committed",
            RejectReason::StaleBase { .. } => "stale_base",
            RejectReason::SegmentClosed => "segment_closed",
            RejectReason::WrongVehicle => "wrong_vehicle",
            RejectReason::Schema => "schema",
            RejectReason::IdMismatch => "id_mismatch",
            RejectReason::ExpiredJob => "expired_job",
        }
    }
}

/// Why a result is quarantined: kept out of the commit path but retained for
/// diagnosis rather than dropped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineReason {
    /// Same logical job identity as one already accepted, but different
    /// semantic content. Raised by the worker after comparing against the
    /// prepared bytes — the pure validator cannot see content, so it never
    /// emits this itself.
    ConflictingContent,
    /// A second result for a job whose commit is already in flight. The first
    /// answer won; a same-id redelivery after prepare is safe to commit only if
    /// byte-identical, which cannot be checked here, so it is quarantined.
    DuplicateAfterCommit,
}

impl QuarantineReason {
    /// A bounded, low-cardinality metric label; one stable string per variant.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            QuarantineReason::ConflictingContent => "conflicting_content",
            QuarantineReason::DuplicateAfterCommit => "duplicate_after_commit",
        }
    }
}

/// Classify one solve `result` against a `vehicle`'s current state on the
/// partition `partition` this worker owns.
///
/// The checks run in strict order so the most authoritative disqualifier wins:
///
/// 1. **Envelope integrity.** [`SolveResult::verify`] confirms the echoed id is
///    the one the identity hashes to ([`RejectReason::IdMismatch`] otherwise),
///    then the schema must be the one this build serves
///    ([`RejectReason::Schema`]).
/// 2. **Addressing.** The result's vehicle must map to `partition`
///    ([`RejectReason::WrongVehicle`] otherwise).
/// 3. **Segment continuity.** If a checkpoint is loaded and the result carries
///    a base, its segment must equal the checkpoint's, else the segment has
///    been closed by a reset ([`RejectReason::SegmentClosed`]).
/// 4. **Job matching.** With an active job of the same id: accept it, unless a
///    commit is already in flight ([`QuarantineReason::DuplicateAfterCommit`]).
///    With an active job of a different id, compare observations — an older one
///    is committed or stale, a newer one is early (park), an equal one with a
///    different id means the base diverged ([`RejectReason::StaleBase`]).
///    With no active job, an observation at or below the committed input is
///    already decided (committed, or [`RejectReason::ExpiredJob`] for the exact
///    committed head), and anything newer is early.
///
/// It is a pure function: the worker performs the commit / park / ack / retain
/// the verdict implies. Content-level conflicts between two same-id results are
/// the worker's to detect against the prepared bytes; this function only sees
/// identities.
pub fn validate<'a, E, H>(
    vehicle: &'a VehicleState<E, H>,
    result: &SolveResult<E>,
    partition: u16,
) -> Verdict<'a>
where
    E: Entry,
    H: AckHandle,
{
    // 1. Envelope integrity. A crossed or corrupted id, or a schema this build
    //    does not serve, is rejected before any state is consulted.
    if result.verify().is_err() {
        return Verdict::Reject(RejectReason::IdMismatch);
    }
    if result.identity.schema != SCHEMA_VERSION {
        return Verdict::Reject(RejectReason::Schema);
    }

    // 2. Addressing. A result whose vehicle does not map to this partition was
    //    delivered to the wrong owner.
    if result.partition() != partition {
        return Verdict::Reject(RejectReason::WrongVehicle);
    }

    // 3. Segment continuity. A loaded checkpoint fixes the vehicle's current
    //    segment; a result carrying a base from a different segment belongs to
    //    a generation the vehicle has closed. (A base-less result is left to
    //    the observation checks below — it cannot name a stale segment.)
    if let (Some(cp), Some(base)) = (vehicle.checkpoint.present(), result.identity.base)
        && base.segment != cp.segment
    {
        return Verdict::Reject(RejectReason::SegmentClosed);
    }

    // 4. Job matching.
    match &vehicle.active {
        // The answer to the job in flight. Accept it — unless a commit is
        // already in flight for it, in which case the first answer won and this
        // same-id redelivery is quarantined (its bytes cannot be compared here).
        Some(active) if active.id == result.job => {
            if vehicle.committing {
                Verdict::Quarantine(QuarantineReason::DuplicateAfterCommit)
            } else {
                Verdict::Accept { job: active }
            }
        }
        // A different job is in flight. The result's place is decided by its
        // observation relative to the active one.
        Some(active) => match result.identity.observation.cmp(&active.observation) {
            // Older than the job in flight: already committed if it is at or
            // below the last committed input, otherwise a stale base for a
            // superseded observation.
            Ordering::Less => Verdict::Reject(older_reject(vehicle, active, result)),
            // Newer than the job in flight: this owner must have dispatched it
            // before a crash; replay will re-create it. Park until then.
            Ordering::Greater => Verdict::Park,
            // The same observation but a different id: the base diverged (for
            // example a new segment opened after state loss).
            Ordering::Equal => Verdict::Reject(RejectReason::StaleBase {
                expected: active.identity.base,
                got: result.identity.base,
            }),
        },
        // No job in flight. A loaded checkpoint decides obsolescence; without
        // one the vehicle has not been reconstructed yet, so the result is
        // early.
        None => match vehicle.checkpoint.present() {
            Some(cp) => match result.identity.observation.cmp(&cp.last_input) {
                Ordering::Less => Verdict::Reject(RejectReason::Committed),
                Ordering::Equal => Verdict::Reject(RejectReason::ExpiredJob),
                Ordering::Greater => Verdict::Park,
            },
            None => Verdict::Park,
        },
    }
}

/// The reject reason for a result older than the vehicle's in-flight job: it is
/// [`RejectReason::Committed`] when the result is at or below the last committed
/// input, otherwise a [`RejectReason::StaleBase`] carrying the base the vehicle
/// currently works from versus the one the result was solved against.
fn older_reject<E, H>(
    vehicle: &VehicleState<E, H>,
    active: &ActiveJob,
    result: &SolveResult<E>,
) -> RejectReason
where
    E: Entry,
    H: AckHandle,
{
    if let Some(cp) = vehicle.checkpoint.present()
        && result.identity.observation <= cp.last_input
    {
        return RejectReason::Committed;
    }
    RejectReason::StaleBase {
        expected: active.identity.base,
        got: result.identity.base,
    }
}

/// Count nothing but report the earliest layer a solve would rewrite: the
/// smallest layer timestamp in `diff` at or below the checkpoint's finality
/// watermark, or `None` when the checkpoint is absent, has no watermark, or the
/// diff touches only unfinalized history.
///
/// Layers at or below [`VehicleCheckpoint::finalized_through`] are settled and
/// must never be re-emitted by a later revision. The commit coordinator strips
/// such layers before publishing; this separate query lets the worker observe
/// and count them without a bundled return riding along the commit path.
#[must_use]
pub fn finality_conflict<E: Entry>(
    checkpoint: Option<&VehicleCheckpoint<E>>,
    diff: &MatchedDiff<E>,
) -> Option<i64> {
    let cutoff = checkpoint?.finalized_through?;
    diff.layers
        .iter()
        .map(|layer| layer.timestamp)
        .filter(|&ts| ts <= cutoff)
        .min()
}

#[cfg(test)]
mod tests {
    use alloc::collections::VecDeque;
    use core::time::Duration;

    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};
    use routers_transition::matcher::Trip;
    use tokio::time::Instant;

    use super::*;
    use crate::event::{MatchedLayer, VehicleId};
    use crate::orchestrator::admission::{Admission, AdmissionConfig};
    use crate::orchestrator::scheduler::CheckpointState;
    use crate::partition::partition_of;
    use crate::protocol::ids::{
        GraphVersion, JobId, ObservationId, RegionId, Revision, SCHEMA_VERSION, SegmentId,
    };
    use crate::protocol::job::JobIdentity;
    use crate::protocol::result::{SolveOutcome, SolveResult};

    /// A minimal [`AckHandle`]; the validator never acks, so it only needs to
    /// exist as the vehicle's handle type.
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

    /// Shared setup: an admission controller (the only source of real
    /// [`Permit`](crate::orchestrator::admission::Permit)s, which `ActiveJob`
    /// needs) plus a region and a monotonic base instant.
    struct Fixture {
        admission: Admission,
        region: RegionId,
        base: Instant,
    }

    impl Fixture {
        fn new() -> Self {
            let region = RegionId::new("r1").unwrap();
            let admission = Admission::new(AdmissionConfig::default(), core::iter::once(&region));
            Self {
                admission,
                region,
                base: Instant::now(),
            }
        }

        fn obs(&self, seq: u64) -> ObservationId {
            ObservationId {
                partition: 7,
                sequence: seq,
            }
        }

        /// An identity for `vehicle` whose head is observation `seq`, resuming
        /// from `base` (segment/revision) — `None` for a fresh vehicle.
        fn identity(&self, vehicle: u64, seq: u64, base: Option<BaseState>) -> JobIdentity {
            JobIdentity {
                schema: SCHEMA_VERSION,
                vehicle_id: VehicleId(vehicle),
                observation: self.obs(seq),
                base,
                graph: GraphVersion::new("g1").unwrap(),
                region: self.region.clone(),
            }
        }

        /// A well-formed result (id matches identity) for `vehicle`/`seq`.
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

        /// An active job for `vehicle` solving observation `seq` from `base`.
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

        /// A present checkpoint whose last committed input is `last_seq`, in
        /// segment `segment`, finalized through `finalized`.
        fn checkpoint(
            &self,
            last_seq: u64,
            segment: u64,
            finalized: Option<i64>,
        ) -> VehicleCheckpoint<MockEntryId> {
            VehicleCheckpoint {
                trip: Trip::new(),
                last_input: self.obs(last_seq),
                revision: Revision(last_seq),
                segment: SegmentId(segment),
                finalized_through: finalized,
                graph: GraphVersion::new("g1").unwrap(),
                schema: SCHEMA_VERSION,
                region: self.region.clone(),
            }
        }

        /// A vehicle in a chosen state. All the levers the decision matrix
        /// exercises are set here so each test reads as one row.
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

    /// The partition a validator must be told it owns for `vehicle` to be
    /// addressed correctly.
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
    fn accepts_the_answer_to_the_active_job() {
        let fx = Fixture::new();
        let job = fx.job(1, 10, None);
        let want_id = job.id;
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(job), false);
        let result = fx.result(1, 10, None);

        match validate(&vehicle, &result, part(1)) {
            Verdict::Accept { job } => assert_eq!(job.id, want_id),
            other => panic!("expected Accept, got {other:?}"),
        }
    }

    #[test]
    fn accepts_a_resumed_job_when_segments_agree() {
        let fx = Fixture::new();
        // Vehicle committed at segment 5; the in-flight job resumes from it.
        let checkpoint = CheckpointState::Present(fx.checkpoint(9, 5, None));
        let job = fx.job(1, 10, Some(base(9, 5)));
        let vehicle = fx.vehicle(checkpoint, Some(job), false);
        let result = fx.result(1, 10, Some(base(9, 5)));

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Accept { .. }
        ));
    }

    #[test]
    fn quarantines_a_duplicate_once_a_commit_is_in_flight() {
        let fx = Fixture::new();
        let job = fx.job(1, 10, None);
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(job), true);
        let result = fx.result(1, 10, None);

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Quarantine(QuarantineReason::DuplicateAfterCommit)
        ));
    }

    #[test]
    fn rejects_a_crossed_envelope_before_touching_state() {
        let fx = Fixture::new();
        let identity = fx.identity(1, 10, None);
        // A forged id that the identity does not hash to.
        let forged = SolveResult::<MockEntryId> {
            job: JobId(0),
            identity,
            outcome: SolveOutcome::Unanchored,
            solved_at_us: 0,
        };
        let vehicle = fx.vehicle(CheckpointState::Unloaded, None, false);

        assert!(matches!(
            validate(&vehicle, &forged, part(1)),
            Verdict::Reject(RejectReason::IdMismatch)
        ));
    }

    #[test]
    fn rejects_a_foreign_schema() {
        let fx = Fixture::new();
        // Build an identity on a different schema; its id still matches so the
        // envelope integrity check passes to the schema gate.
        let mut identity = fx.identity(1, 10, None);
        identity.schema = crate::protocol::ids::SchemaVersion(SCHEMA_VERSION.0 + 1);
        let result = SolveResult::<MockEntryId> {
            job: identity.job_id(),
            identity,
            outcome: SolveOutcome::Unanchored,
            solved_at_us: 0,
        };
        let vehicle = fx.vehicle(CheckpointState::Unloaded, None, false);

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Reject(RejectReason::Schema)
        ));
    }

    #[test]
    fn rejects_a_result_addressed_to_the_wrong_partition() {
        let fx = Fixture::new();
        let vehicle = fx.vehicle(CheckpointState::Unloaded, None, false);
        let result = fx.result(1, 10, None);
        // Any partition other than the vehicle's own.
        let wrong = part(1) ^ 1;

        assert!(matches!(
            validate(&vehicle, &result, wrong),
            Verdict::Reject(RejectReason::WrongVehicle)
        ));
    }

    #[test]
    fn rejects_a_result_from_a_closed_segment() {
        let fx = Fixture::new();
        // Checkpoint is in segment 5; the result resumes from segment 4.
        let checkpoint = CheckpointState::Present(fx.checkpoint(9, 5, None));
        let vehicle = fx.vehicle(checkpoint, None, false);
        let result = fx.result(1, 10, Some(base(9, 4)));

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Reject(RejectReason::SegmentClosed)
        ));
    }

    #[test]
    fn parks_an_early_result_with_no_active_job() {
        let fx = Fixture::new();
        // Checkpoint committed input 9; the result is for a newer observation
        // whose job replay has not re-dispatched yet.
        let checkpoint = CheckpointState::Present(fx.checkpoint(9, 5, None));
        let vehicle = fx.vehicle(checkpoint, None, false);
        let result = fx.result(1, 20, Some(base(9, 5)));

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Park
        ));
    }

    #[test]
    fn parks_when_the_checkpoint_is_not_loaded_yet() {
        let fx = Fixture::new();
        let vehicle = fx.vehicle(CheckpointState::Unloaded, None, false);
        let result = fx.result(1, 20, None);

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Park
        ));
    }

    #[test]
    fn parks_a_future_result_while_an_older_job_is_in_flight() {
        let fx = Fixture::new();
        // Active job solves observation 10; a result for observation 20 is a
        // job this owner dispatched before a crash — park it.
        let job = fx.job(1, 10, None);
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(job), false);
        let result = fx.result(1, 20, None);

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Park
        ));
    }

    #[test]
    fn rejects_a_committed_result_with_no_active_job() {
        let fx = Fixture::new();
        // Committed input is 9; a result at exactly 8 is already superseded.
        let checkpoint = CheckpointState::Present(fx.checkpoint(9, 5, None));
        let vehicle = fx.vehicle(checkpoint, None, false);
        let result = fx.result(1, 8, Some(base(9, 5)));

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Reject(RejectReason::Committed)
        ));
    }

    #[test]
    fn rejects_the_already_resolved_committed_head() {
        let fx = Fixture::new();
        // The result is for the exact observation the checkpoint last committed
        // (9) with no job in flight: that job already resolved (e.g. by the
        // deadline handler).
        let checkpoint = CheckpointState::Present(fx.checkpoint(9, 5, None));
        let vehicle = fx.vehicle(checkpoint, None, false);
        let result = fx.result(1, 9, Some(base(9, 5)));

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Reject(RejectReason::ExpiredJob)
        ));
    }

    #[test]
    fn rejects_a_stale_base_for_an_older_uncommitted_observation() {
        let fx = Fixture::new();
        // Active job solves observation 10; a result for observation 8 that is
        // still above the committed input (5) is a stale base, not committed.
        let checkpoint = CheckpointState::Present(fx.checkpoint(5, 3, None));
        let job = fx.job(1, 10, Some(base(5, 3)));
        let vehicle = fx.vehicle(checkpoint, Some(job), false);
        let result = fx.result(1, 8, Some(base(5, 3)));

        match validate(&vehicle, &result, part(1)) {
            Verdict::Reject(RejectReason::StaleBase { expected, got }) => {
                assert_eq!(expected, Some(base(5, 3)));
                assert_eq!(got, Some(base(5, 3)));
            }
            other => panic!("expected StaleBase, got {other:?}"),
        }
    }

    #[test]
    fn rejects_committed_when_an_older_result_is_at_or_below_the_input() {
        let fx = Fixture::new();
        // Active job at 10, committed input at 8; a result at exactly 8 is
        // committed even though a different job is in flight.
        let checkpoint = CheckpointState::Present(fx.checkpoint(8, 3, None));
        let job = fx.job(1, 10, Some(base(8, 3)));
        let vehicle = fx.vehicle(checkpoint, Some(job), false);
        let result = fx.result(1, 8, Some(base(8, 3)));

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Reject(RejectReason::Committed)
        ));
    }

    #[test]
    fn rejects_a_diverged_base_at_the_same_observation() {
        let fx = Fixture::new();
        // Same observation as the active job, but a different base (a new
        // segment opened after state loss) makes it a different id.
        let job = fx.job(1, 10, Some(base(9, 5)));
        let vehicle = fx.vehicle(CheckpointState::Unloaded, Some(job), false);
        let result = fx.result(1, 10, Some(base(9, 6)));

        match validate(&vehicle, &result, part(1)) {
            Verdict::Reject(RejectReason::StaleBase { expected, got }) => {
                assert_eq!(expected, Some(base(9, 5)));
                assert_eq!(got, Some(base(9, 6)));
            }
            other => panic!("expected StaleBase, got {other:?}"),
        }
    }

    /// Every reject/quarantine reason has a distinct, stable label.
    #[test]
    fn labels_are_distinct_and_stable() {
        let rejects = [
            (RejectReason::Committed, "committed"),
            (
                RejectReason::StaleBase {
                    expected: None,
                    got: None,
                },
                "stale_base",
            ),
            (RejectReason::SegmentClosed, "segment_closed"),
            (RejectReason::WrongVehicle, "wrong_vehicle"),
            (RejectReason::Schema, "schema"),
            (RejectReason::IdMismatch, "id_mismatch"),
            (RejectReason::ExpiredJob, "expired_job"),
        ];
        let mut seen = std::collections::HashSet::new();
        for (reason, label) in rejects {
            assert_eq!(reason.label(), label);
            assert!(seen.insert(label), "duplicate reject label {label}");
        }
        assert_eq!(seen.len(), 7);

        for (reason, label) in [
            (QuarantineReason::ConflictingContent, "conflicting_content"),
            (
                QuarantineReason::DuplicateAfterCommit,
                "duplicate_after_commit",
            ),
        ] {
            assert_eq!(reason.label(), label);
        }

        assert_eq!(Verdict::Park.label(), "park");
        assert_eq!(Verdict::Reject(RejectReason::Schema).label(), "reject");
        assert_eq!(
            Verdict::Quarantine(QuarantineReason::ConflictingContent).label(),
            "quarantine"
        );
    }

    fn layer(timestamp: i64) -> MatchedLayer<MockEntryId> {
        MatchedLayer {
            timestamp,
            edge: Edge {
                source: MockEntryId(0),
                target: MockEntryId(0),
                weight: 0,
                id: DirectionAwareEdgeId::new(MockEntryId(0)),
            },
            position: Point::new(0.0, 0.0),
            path: Vec::new(),
        }
    }

    fn diff(timestamps: &[i64]) -> MatchedDiff<MockEntryId> {
        MatchedDiff {
            revision: 1,
            downgraded: false,
            layers: timestamps.iter().copied().map(layer).collect(),
        }
    }

    #[test]
    fn finality_conflict_finds_the_earliest_offender() {
        let fx = Fixture::new();
        let checkpoint = fx.checkpoint(9, 5, Some(100));
        // Two layers at or below the watermark (100): 40 and 80; 40 is earliest.
        let d = diff(&[80, 200, 40, 300]);
        assert_eq!(finality_conflict(Some(&checkpoint), &d), Some(40));
    }

    #[test]
    fn finality_conflict_none_when_all_layers_are_unfinalized() {
        let fx = Fixture::new();
        let checkpoint = fx.checkpoint(9, 5, Some(100));
        let d = diff(&[101, 200, 300]);
        assert_eq!(finality_conflict(Some(&checkpoint), &d), None);
    }

    #[test]
    fn finality_conflict_none_without_a_watermark_or_checkpoint() {
        let fx = Fixture::new();
        let d = diff(&[1, 2, 3]);
        // No finality watermark set.
        let checkpoint = fx.checkpoint(9, 5, None);
        assert_eq!(finality_conflict(Some(&checkpoint), &d), None);
        // No checkpoint at all.
        assert_eq!(finality_conflict(None, &d), None);
    }
}
