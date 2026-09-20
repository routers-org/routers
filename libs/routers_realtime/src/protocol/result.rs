//! `SolveResult`: the partitioned solve-result message. A matcher answers one
//! [`SolveJob`] with exactly one [`SolveResult`] on the vehicle's result
//! partition, echoing the job's [`JobId`] and [`JobIdentity`] so the owning
//! orchestrator validates without a lookup; the [`JobId`] is the `Nats-Msg-Id`.
//!
//! Every outcome other than [`SolveOutcome::Solved`] is terminal; only
//! Unanchored and Disconnected are nominal terminals that leave committed state
//! intact.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use routers_network::Entry;
use routers_transition::matcher::Trip;

use crate::bus::{Wire, postcard_wire};
use crate::event::MatchedDiff;
use crate::partition::partition_of;
use crate::protocol::ids::{GraphVersion, JobId};
use crate::protocol::job::{JobIdentity, JobProof, SolveJob};
use crate::protocol::output::TerminalReason;

/// The outcome of solving one job: either the matched emission with the resume
/// state that follows it, or one of the terminal conditions that close the job.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub enum SolveOutcome<E: Entry> {
    /// The solve produced an emission: `diff` is the matched history to publish,
    /// `trip` the resume state, and `converged_through` the microsecond
    /// timestamp up to which the match is settled, or `None` when nothing converged.
    Solved {
        diff: MatchedDiff<E>,
        trip: Trip<E>,
        converged_through: Option<i64>,
    },

    /// No layer could be anchored to the network. Nominal.
    Unanchored,

    /// The network could not bridge the trace into a connected route. Nominal.
    Disconnected,

    /// The trace left the region's loaded coverage; `cell` names the unserved cell.
    UnsupportedCoverage { cell: String },

    /// The job was cut against a graph the matcher does not serve.
    VersionMismatch {
        expected: GraphVersion,
        got: GraphVersion,
    },

    /// The decoded job exceeded the matcher's size bound.
    Oversized { bytes: usize, limit: usize },

    /// The solve failed for a reason that is the matcher's own fault; `reason`
    /// is a log description, never a metric label.
    Internal { reason: String },
}

impl<E: Entry> SolveOutcome<E> {
    /// A bounded, low-cardinality label for metrics and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            SolveOutcome::Solved { .. } => "solved",
            SolveOutcome::Unanchored => "unanchored",
            SolveOutcome::Disconnected => "disconnected",
            SolveOutcome::UnsupportedCoverage { .. } => "unsupported_coverage",
            SolveOutcome::VersionMismatch { .. } => "version_mismatch",
            SolveOutcome::Oversized { .. } => "oversized",
            SolveOutcome::Internal { .. } => "internal",
        }
    }

    /// Whether the solve produced an emission (only [`SolveOutcome::Solved`]).
    pub fn is_success(&self) -> bool {
        matches!(self, SolveOutcome::Solved { .. })
    }

    /// Whether this is an expected terminal that leaves committed state untouched.
    pub fn is_nominal(&self) -> bool {
        matches!(self, SolveOutcome::Unanchored | SolveOutcome::Disconnected)
    }

    /// Whether this outcome closes the job. Every non-success outcome is
    /// terminal, so this is exactly `!is_success()`.
    pub fn is_terminal(&self) -> bool {
        !self.is_success()
    }

    /// The [`TerminalReason`] the commit path records for this outcome, or
    /// `None` for [`SolveOutcome::Solved`]. `Oversized` maps to
    /// [`TerminalReason::Internal`].
    pub fn terminal_reason(&self) -> Option<TerminalReason> {
        match self {
            SolveOutcome::Solved { .. } => None,
            SolveOutcome::Unanchored => Some(TerminalReason::Unanchored),
            SolveOutcome::Disconnected => Some(TerminalReason::Disconnected),
            SolveOutcome::UnsupportedCoverage { .. } => Some(TerminalReason::UnsupportedCoverage),
            SolveOutcome::VersionMismatch { .. } => Some(TerminalReason::VersionMismatch),
            SolveOutcome::Oversized { .. } => Some(TerminalReason::Internal),
            SolveOutcome::Internal { .. } => Some(TerminalReason::Internal),
        }
    }
}

/// One matcher's answer to one job, addressed to the vehicle's result partition.
/// `job`, `identity`, and its compact [`JobProof`] are copied off the
/// [`SolveJob`] so the reader can [`verify`](SolveResult::verify) without
/// touching any store or duplicating the continuation payload.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct SolveResult<E: Entry> {
    /// The job this answers; equals `proof.job_id()` and is the stream's
    /// `Nats-Msg-Id`.
    pub job: JobId,
    /// The durable job context, echoed for routing and store comparisons.
    pub identity: JobIdentity,
    /// The authenticated full solve context, compactly represented so this
    /// result can verify `job` without carrying the continuation twice.
    pub proof: JobProof,
    /// What the solve produced.
    pub outcome: SolveOutcome<E>,
    /// When the matcher finished the solve (absolute unix microseconds).
    pub solved_at_us: i64,
}

impl<E: Entry> SolveResult<E> {
    /// Build a result for `job`, copying its id and identity so the two can
    /// never drift. `solved_at_us` is the absolute unix-micros completion time.
    pub fn new(job: &SolveJob<E>, outcome: SolveOutcome<E>, solved_at_us: i64) -> Self {
        Self {
            job: job.id,
            identity: job.identity.clone(),
            proof: job.proof(),
            outcome,
            solved_at_us,
        }
    }

    /// Check that the echoed identity agrees with the proof and that `job` is
    /// the proof's digest. A mismatch means the envelope was corrupted or
    /// crossed with another job.
    pub fn verify(&self) -> Result<(), ResultError> {
        if self.identity != self.proof.identity {
            return Err(ResultError::IdentityMismatch);
        }
        let computed = self.proof.job_id();
        if self.job != computed {
            return Err(ResultError::IdMismatch {
                claimed: self.job,
                computed,
            });
        }
        Ok(())
    }

    /// The broker dedup key for the result stream: the job id hex.
    pub fn msg_id(&self) -> String {
        self.job.to_string()
    }

    /// The result subject's partition: the vehicle's fleet-wide partition.
    pub fn partition(&self) -> u16 {
        partition_of(self.identity.vehicle_id) as u16
    }
}

impl<E: Entry + serde::de::DeserializeOwned> SolveResult<E> {
    /// Decode from the wire and immediately [`verify`](Self::verify). Fails
    /// with [`ResultError::Decode`] on malformed bytes, or
    /// [`ResultError::IdMismatch`] when the decoded id does not match its identity.
    pub fn decode_verified(bytes: &[u8]) -> Result<Self, ResultError> {
        let result = <Self as Wire>::decode(bytes).map_err(ResultError::Decode)?;
        result.verify()?;
        Ok(result)
    }
}

postcard_wire!(SolveResult<E: Entry>);

/// Why a [`SolveResult`] could not be trusted.
#[derive(Debug, Error)]
pub enum ResultError {
    /// The echoed `job` id is not the id its proof hashes to.
    #[error("result job id {claimed} does not match its proof's job id {computed}")]
    IdMismatch { claimed: JobId, computed: JobId },
    /// The public routing identity did not agree with the authenticated proof.
    #[error("result identity does not match its job proof")]
    IdentityMismatch,
    /// The bytes did not decode into a [`SolveResult`].
    #[error("could not decode solve result")]
    Decode(#[source] anyhow::Error),
}

#[cfg(test)]
mod tests {
    use super::*;

    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::Continuation;

    use crate::event::VehicleId;
    use crate::partition;
    use crate::protocol::ids::{
        Lane, ObservationId, RegionId, Revision, SCHEMA_VERSION, SegmentId,
    };
    use crate::protocol::job::BaseState;

    fn sample_identity(vehicle: u64) -> JobIdentity {
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(vehicle),
            observation: ObservationId {
                partition: 3,
                sequence: 99,
            },
            base: Some(BaseState {
                revision: Revision(41),
                segment: SegmentId(7),
            }),
            graph: GraphVersion::new("europe-2026-09").unwrap(),
            region: RegionId::new("paris").unwrap(),
        }
    }

    fn sample_job(identity: JobIdentity) -> SolveJob<MockEntryId> {
        SolveJob::new(
            identity,
            Lane::DEFAULT,
            1_726_000_000_000_000,
            Continuation::Restart { fresh: Vec::new() },
        )
    }

    fn every_outcome() -> Vec<SolveOutcome<MockEntryId>> {
        vec![
            SolveOutcome::Solved {
                diff: MatchedDiff {
                    revision: 3,
                    downgraded: false,
                    layers: Vec::new(),
                },
                trip: Trip::new(),
                converged_through: Some(123),
            },
            SolveOutcome::Unanchored,
            SolveOutcome::Disconnected,
            SolveOutcome::UnsupportedCoverage {
                cell: "gcpv".to_owned(),
            },
            SolveOutcome::VersionMismatch {
                expected: GraphVersion::new("europe-2026-09").unwrap(),
                got: GraphVersion::new("europe-2026-08").unwrap(),
            },
            SolveOutcome::Oversized {
                bytes: 5 << 20,
                limit: 4 << 20,
            },
            SolveOutcome::Internal {
                reason: "solver panicked".to_owned(),
            },
        ]
    }

    fn expectations(
        outcome: &SolveOutcome<MockEntryId>,
    ) -> (&'static str, bool, bool, bool, Option<TerminalReason>) {
        match outcome {
            SolveOutcome::Solved { .. } => ("solved", true, false, false, None),
            SolveOutcome::Unanchored => (
                "unanchored",
                false,
                true,
                true,
                Some(TerminalReason::Unanchored),
            ),
            SolveOutcome::Disconnected => (
                "disconnected",
                false,
                true,
                true,
                Some(TerminalReason::Disconnected),
            ),
            SolveOutcome::UnsupportedCoverage { .. } => (
                "unsupported_coverage",
                false,
                false,
                true,
                Some(TerminalReason::UnsupportedCoverage),
            ),
            SolveOutcome::VersionMismatch { .. } => (
                "version_mismatch",
                false,
                false,
                true,
                Some(TerminalReason::VersionMismatch),
            ),
            SolveOutcome::Oversized { .. } => (
                "oversized",
                false,
                false,
                true,
                Some(TerminalReason::Internal),
            ),
            SolveOutcome::Internal { .. } => (
                "internal",
                false,
                false,
                true,
                Some(TerminalReason::Internal),
            ),
        }
    }

    #[test]
    fn classification_table_covers_every_variant() {
        let mut kinds = std::collections::HashSet::new();
        for outcome in every_outcome() {
            let (kind, success, nominal, terminal, reason) = expectations(&outcome);
            assert_eq!(outcome.kind(), kind, "kind for {kind}");
            assert_eq!(outcome.is_success(), success, "is_success for {kind}");
            assert_eq!(outcome.is_nominal(), nominal, "is_nominal for {kind}");
            assert_eq!(outcome.is_terminal(), terminal, "is_terminal for {kind}");
            // Compare via Debug since `TerminalReason` may not derive `PartialEq`.
            assert_eq!(
                format!("{:?}", outcome.terminal_reason()),
                format!("{reason:?}"),
                "terminal_reason for {kind}"
            );
            assert_eq!(outcome.is_terminal(), !outcome.is_success());
            assert!(!(outcome.is_success() && outcome.is_nominal()));
            kinds.insert(kind);
        }
        assert_eq!(kinds.len(), 7, "every variant has a distinct kind label");
    }

    #[test]
    fn solved_round_trips_over_the_wire() {
        let identity = sample_identity(0x1234_5678);
        let job = sample_job(identity);
        let outcome = SolveOutcome::Solved {
            diff: MatchedDiff {
                revision: 7,
                downgraded: false,
                layers: Vec::new(),
            },
            trip: Trip::new(),
            converged_through: Some(1_726_000_000_000_000),
        };
        let result = SolveResult::new(&job, outcome, 1_726_000_000_500_000);

        assert_eq!(result.job, job.id);
        assert_eq!(result.identity, job.identity);
        result.verify().expect("a well-formed result verifies");

        let bytes = result.encode().expect("encode");
        let decoded = SolveResult::<MockEntryId>::decode_verified(&bytes).expect("decode + verify");

        assert_eq!(decoded.job, result.job);
        assert_eq!(decoded.identity, result.identity);
        assert_eq!(decoded.solved_at_us, 1_726_000_000_500_000);
        match &decoded.outcome {
            SolveOutcome::Solved {
                diff,
                converged_through,
                ..
            } => {
                assert_eq!(diff.revision, 7);
                assert_eq!(*converged_through, Some(1_726_000_000_000_000));
            }
            other => panic!("expected Solved, got {}", other.kind()),
        }
        assert_eq!(decoded.encode().expect("re-encode"), bytes);
    }

    #[test]
    fn error_variants_round_trip_over_the_wire() {
        let identity = sample_identity(0xabc_def);
        let job = sample_job(identity);

        let coverage = SolveResult::new(
            &job,
            SolveOutcome::UnsupportedCoverage {
                cell: "u09t".to_owned(),
            },
            10,
        );
        let bytes = coverage.encode().expect("encode coverage");
        let decoded = SolveResult::<MockEntryId>::decode_verified(&bytes).expect("decode coverage");
        match decoded.outcome {
            SolveOutcome::UnsupportedCoverage { cell } => assert_eq!(cell, "u09t"),
            other => panic!("expected UnsupportedCoverage, got {}", other.kind()),
        }

        let oversized = SolveResult::new(
            &job,
            SolveOutcome::Oversized {
                bytes: 9_000_000,
                limit: 4 << 20,
            },
            20,
        );
        let bytes = oversized.encode().expect("encode oversized");
        let decoded =
            SolveResult::<MockEntryId>::decode_verified(&bytes).expect("decode oversized");
        match decoded.outcome {
            SolveOutcome::Oversized { bytes, limit } => {
                assert_eq!(bytes, 9_000_000);
                assert_eq!(limit, 4 << 20);
            }
            other => panic!("expected Oversized, got {}", other.kind()),
        }
    }

    #[test]
    fn verify_catches_a_job_identity_mismatch() {
        let identity = sample_identity(7);
        let job = sample_job(identity.clone());
        let mut forged = SolveResult::new(&job, SolveOutcome::Unanchored, 0);
        forged.job = JobId(0);
        // ResultError isn't PartialEq; match on the variant.
        match forged.verify() {
            Err(ResultError::IdMismatch { claimed, computed }) => {
                assert_eq!(claimed, JobId(0));
                assert_eq!(computed, job.id);
            }
            other => panic!("expected IdMismatch, got {other:?}"),
        }

        let bytes = forged.encode().expect("encode");
        match SolveResult::<MockEntryId>::decode_verified(&bytes) {
            Err(ResultError::IdMismatch { claimed, .. }) => assert_eq!(claimed, JobId(0)),
            other => panic!("expected IdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_catches_tampered_proof_fields() {
        let job = sample_job(sample_identity(7));
        let mut target_tampered = SolveResult::new(&job, SolveOutcome::Unanchored, 0);
        target_tampered.proof.freshness_target_us += 1;
        assert!(matches!(
            target_tampered.verify(),
            Err(ResultError::IdMismatch { .. })
        ));

        let mut identity_tampered = SolveResult::new(&job, SolveOutcome::Unanchored, 0);
        identity_tampered.identity.graph = GraphVersion::new("other-graph").unwrap();
        assert!(matches!(
            identity_tampered.verify(),
            Err(ResultError::IdentityMismatch)
        ));
    }

    #[test]
    fn decode_verified_rejects_malformed_bytes() {
        let err = SolveResult::<MockEntryId>::decode_verified(&[0xff]);
        assert!(matches!(err, Err(ResultError::Decode(_))), "got {err:?}");
    }

    #[test]
    fn partition_agrees_with_partition_of() {
        for vehicle in [1u64, 0xdead_beef, 0x1234_5678_9abc_def0, u64::MAX] {
            let identity = sample_identity(vehicle);
            let job = sample_job(identity);
            let result = SolveResult::new(&job, SolveOutcome::Unanchored, 0);
            let expected = partition::partition_of(VehicleId(vehicle)) as u16;
            assert_eq!(result.partition(), expected);
            assert!(result.partition() < partition::PARTITIONS as u16);
        }
    }

    #[test]
    fn msg_id_is_the_job_hex() {
        let identity = sample_identity(42);
        let job = sample_job(identity);
        let result = SolveResult::new(&job, SolveOutcome::Unanchored, 0);
        assert_eq!(result.msg_id(), job.id.to_string());
        assert_eq!(result.msg_id().len(), 32);
    }
}
