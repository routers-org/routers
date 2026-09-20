//! Orchestrator: the pure result reader and validator.
//!
//! Classifies each solve result against the vehicle's current state as commit,
//! park (early — replay has not re-created its job), reject (obsolete), or
//! quarantine (a same-id redelivery whose bytes cannot be compared here). Pure
//! and I/O-free: the worker performs the side effect the verdict implies.

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
#[derive(Debug)]
pub enum Verdict<'a> {
    /// The expected result for the vehicle's active job: commit it.
    Accept {
        /// The in-flight job this result answers.
        job: &'a ActiveJob,
    },
    /// No active job yet for this observation, but it is newer than the last
    /// committed input: replay has not reached it. Park and revisit when the job is dispatched.
    Park,
    /// Definitively obsolete: ack and drop.
    Reject(RejectReason),
    /// A same-identity result that must never reach the commit path; kept for diagnosis.
    Quarantine(QuarantineReason),
}

impl Verdict<'_> {
    /// A bounded, low-cardinality label for the verdict class, suitable as a metric dimension.
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
    /// The result's observation is at or below the vehicle's last committed input.
    Committed,
    /// The result resumes from a base the vehicle no longer holds.
    StaleBase {
        /// The base the vehicle expects (its in-flight job's base), if any.
        expected: Option<BaseState>,
        /// The base the result was solved against, if any.
        got: Option<BaseState>,
    },
    /// The result belongs to a continuity segment the vehicle has since closed.
    SegmentClosed,
    /// The result's vehicle does not map to the partition this worker owns.
    WrongVehicle,
    /// The result was produced against a wire schema this build does not serve.
    Schema,
    /// The echoed job id does not match the id its identity hashes to.
    IdMismatch,
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
        }
    }
}

/// Why a result is quarantined: kept out of the commit path but retained for diagnosis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuarantineReason {
    /// Same logical job identity as one already accepted, but different semantic content. Raised by the worker, never by the pure validator.
    ConflictingContent,
    /// A second result for a job whose commit is already in flight; its bytes cannot be compared here.
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
/// `partition` this worker owns.
///
/// Checks run in strict order — envelope integrity, addressing, segment
/// continuity, then job matching — so the most authoritative disqualifier wins.
/// Pure: the worker performs the commit / park / ack / retain the verdict implies.
pub fn validate<'a, E, H>(
    vehicle: &'a VehicleState<E, H>,
    result: &SolveResult<E>,
    partition: u16,
) -> Verdict<'a>
where
    E: Entry,
    H: AckHandle,
{
    if result.verify().is_err() {
        return Verdict::Reject(RejectReason::IdMismatch);
    }
    if result.identity.schema != SCHEMA_VERSION {
        return Verdict::Reject(RejectReason::Schema);
    }

    if result.partition() != partition {
        return Verdict::Reject(RejectReason::WrongVehicle);
    }

    // A base-less result names no segment, so it is left to the observation checks below.
    if let (Some(cp), Some(base)) = (vehicle.checkpoint.present(), result.identity.base)
        && base.segment != cp.segment
    {
        return Verdict::Reject(RejectReason::SegmentClosed);
    }

    match &vehicle.active {
        Some(active) if active.id == result.job => {
            if vehicle.committing {
                Verdict::Quarantine(QuarantineReason::DuplicateAfterCommit)
            } else {
                Verdict::Accept { job: active }
            }
        }
        Some(active) => match result.identity.observation.cmp(&active.observation) {
            Ordering::Less => Verdict::Reject(older_reject(vehicle, active, result)),
            // Newer than the in-flight job: this owner dispatched it before a crash; park until replay re-creates it.
            Ordering::Greater => Verdict::Park,
            Ordering::Equal => Verdict::Reject(RejectReason::StaleBase {
                expected: active.identity.base,
                got: result.identity.base,
            }),
        },
        None => match vehicle.checkpoint.present() {
            Some(cp) => match result.identity.observation.cmp(&cp.last_input) {
                Ordering::Less => Verdict::Reject(RejectReason::Committed),
                Ordering::Equal => Verdict::Reject(RejectReason::Committed),
                Ordering::Greater => Verdict::Park,
            },
            None => Verdict::Park,
        },
    }
}

/// The reject reason for a result older than the vehicle's in-flight job:
/// [`RejectReason::Committed`] at or below the last committed input, else
/// [`RejectReason::StaleBase`].
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

/// Report the earliest layer a solve would rewrite: the smallest layer timestamp
/// in `diff` at or below the checkpoint's finality watermark, or `None` when the
/// checkpoint is absent, has no watermark, or the diff touches only unfinalized history.
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
    use routers_transition::matcher::{Continuation, Trip};
    use tokio::time::Instant;

    use super::*;
    use crate::event::{MatchedLayer, VehicleId};
    use crate::orchestrator::admission::{Admission, AdmissionConfig};
    use crate::orchestrator::scheduler::{CheckpointState, JobReservation};
    use crate::partition::partition_of;
    use crate::protocol::ids::{
        GraphVersion, JobId, Lane, ObservationId, RegionId, Revision, SCHEMA_VERSION, SegmentId,
    };
    use crate::protocol::job::{JobIdentity, SolveJob};
    use crate::protocol::result::{SolveOutcome, SolveResult};

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

        fn job(&self, vehicle: u64, seq: u64, base: Option<BaseState>) -> ActiveJob {
            let solve_job = self.solve_job(vehicle, seq, base);
            ActiveJob {
                id: solve_job.id,
                identity: solve_job.identity,
                observation: self.obs(seq),
                bytes: 100,
                reservation: JobReservation::Admitted(
                    self.admission.try_admit(&self.region, 100).unwrap(),
                ),
                dispatched: self.base,
            }
        }

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
                routing_version: 1,
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
        let job = fx.solve_job(1, 10, None);
        // A forged id that the identity does not hash to.
        let mut forged = SolveResult::new(&job, SolveOutcome::Unanchored, 0);
        forged.job = JobId(0);
        let vehicle = fx.vehicle(CheckpointState::Unloaded, None, false);

        assert!(matches!(
            validate(&vehicle, &forged, part(1)),
            Verdict::Reject(RejectReason::IdMismatch)
        ));
    }

    #[test]
    fn rejects_a_foreign_schema() {
        let fx = Fixture::new();
        // Its id still hashes correctly, so validation reaches the schema gate.
        let mut identity = fx.identity(1, 10, None);
        identity.schema = crate::protocol::ids::SchemaVersion(SCHEMA_VERSION.0 + 1);
        let job = SolveJob::new(
            identity,
            Lane::DEFAULT,
            1_000,
            Continuation::Restart { fresh: Vec::new() },
        );
        let result = SolveResult::new(&job, SolveOutcome::Unanchored, 0);
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
        let wrong = part(1) ^ 1;

        assert!(matches!(
            validate(&vehicle, &result, wrong),
            Verdict::Reject(RejectReason::WrongVehicle)
        ));
    }

    #[test]
    fn rejects_a_result_from_a_closed_segment() {
        let fx = Fixture::new();
        // The checkpoint is in segment 5; the result resumes from the closed segment 4.
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
        // A newer observation whose job replay has not re-dispatched yet.
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
        // A result newer than the in-flight job: dispatched before a crash, so park it.
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
        // The exact committed observation with no job in flight: that job already resolved.
        let checkpoint = CheckpointState::Present(fx.checkpoint(9, 5, None));
        let vehicle = fx.vehicle(checkpoint, None, false);
        let result = fx.result(1, 9, Some(base(9, 5)));

        assert!(matches!(
            validate(&vehicle, &result, part(1)),
            Verdict::Reject(RejectReason::Committed)
        ));
    }

    #[test]
    fn rejects_a_stale_base_for_an_older_uncommitted_observation() {
        let fx = Fixture::new();
        // A result older than the in-flight job but still above the committed input: stale, not committed.
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
        // A result at the committed input is committed even though a different job is in flight.
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
        // Same observation as the active job, but a different base makes it a different id.
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
        ];
        let mut seen = std::collections::HashSet::new();
        for (reason, label) in rejects {
            assert_eq!(reason.label(), label);
            assert!(seen.insert(label), "duplicate reject label {label}");
        }
        assert_eq!(seen.len(), 6);

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
        let checkpoint = fx.checkpoint(9, 5, None);
        assert_eq!(finality_conflict(Some(&checkpoint), &d), None);
        assert_eq!(finality_conflict(None, &d), None);
    }
}
