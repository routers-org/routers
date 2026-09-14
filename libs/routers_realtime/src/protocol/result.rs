//! `SolveResult`: the partitioned solve-result message. (T03)
//!
//! A matcher answers one [`SolveJob`] with exactly one [`SolveResult`],
//! published on the vehicle's result partition so the owning orchestrator
//! worker — which owns that partition and nothing else — is the sole reader.
//! The result echoes the job's [`JobId`] and its whole [`JobIdentity`]: the
//! owner validates that the answer belongs to the job it dispatched without a
//! lookup, and the [`JobId`] doubles as the stream's `Nats-Msg-Id`, so a
//! matcher that retries an ambiguous publish is deduplicated by the broker.
//!
//! Every outcome other than [`SolveOutcome::Solved`] is *terminal*: it closes
//! the job. Only [`SolveOutcome::Unanchored`] and
//! [`SolveOutcome::Disconnected`] are *nominal* terminals — expected answers
//! that leave the vehicle's committed state intact — while the rest record a
//! fault. [`SolveOutcome::terminal_reason`] projects each terminal onto the
//! [`TerminalReason`] the commit path stamps onto a `Terminal` output.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use routers_network::Entry;
use routers_transition::matcher::Trip;

use crate::bus::{Wire, postcard_wire};
use crate::event::MatchedDiff;
use crate::partition::partition_of;
use crate::protocol::ids::{GraphVersion, JobId};
use crate::protocol::job::{JobIdentity, SolveJob};
use crate::protocol::output::TerminalReason;

/// The outcome of solving one job: either the matched emission with the resume
/// state that follows it, or one of the terminal conditions that close the job.
///
/// `E` is the network's entry identifier; it rides along inside the matched
/// diff and trip of a [`SolveOutcome::Solved`] answer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub enum SolveOutcome<E: Entry> {
    /// The solve produced an emission. `diff` is the matched history to
    /// publish, `trip` is the resume state the owner commits for the next
    /// observation, and `converged_through` is the microsecond timestamp of
    /// the convergence layer (the point up to which the match is settled), or
    /// `None` when nothing converged.
    Solved {
        diff: MatchedDiff<E>,
        trip: Trip<E>,
        converged_through: Option<i64>,
    },

    /// No layer could be anchored to the network. Nominal: the trace simply
    /// had nothing to match against.
    Unanchored,

    /// The network could not bridge the trace into a connected route. Nominal.
    Disconnected,

    /// The trace left the region's loaded coverage. `cell` names the
    /// geographic cell that was not served.
    UnsupportedCoverage { cell: String },

    /// The job was cut against a graph the matcher does not serve.
    VersionMismatch {
        expected: GraphVersion,
        got: GraphVersion,
    },

    /// The job's deadline had already passed when the matcher dequeued it.
    DeadlineExpired,

    /// The decoded job exceeded the matcher's size bound. `bytes` is the
    /// decoded size, `limit` the bound it broke.
    Oversized { bytes: usize, limit: usize },

    /// The solve failed for a reason that is the matcher's own fault. `reason`
    /// is a human-facing description for logs, never a metric label.
    Internal { reason: String },
}

impl<E: Entry> SolveOutcome<E> {
    /// A bounded, low-cardinality label for metrics and logs. One stable
    /// string per variant; safe to use as a metric dimension.
    pub fn kind(&self) -> &'static str {
        match self {
            SolveOutcome::Solved { .. } => "solved",
            SolveOutcome::Unanchored => "unanchored",
            SolveOutcome::Disconnected => "disconnected",
            SolveOutcome::UnsupportedCoverage { .. } => "unsupported_coverage",
            SolveOutcome::VersionMismatch { .. } => "version_mismatch",
            SolveOutcome::DeadlineExpired => "deadline_expired",
            SolveOutcome::Oversized { .. } => "oversized",
            SolveOutcome::Internal { .. } => "internal",
        }
    }

    /// Whether the solve produced an emission (only [`SolveOutcome::Solved`]).
    pub fn is_success(&self) -> bool {
        matches!(self, SolveOutcome::Solved { .. })
    }

    /// Whether this is an *expected* terminal that leaves committed state
    /// untouched ([`SolveOutcome::Unanchored`] or [`SolveOutcome::Disconnected`]).
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
    /// [`TerminalReason::Internal`] — an oversize job is the fleet's own
    /// misconfiguration, not a distinct customer-visible cause.
    pub fn terminal_reason(&self) -> Option<TerminalReason> {
        match self {
            SolveOutcome::Solved { .. } => None,
            SolveOutcome::Unanchored => Some(TerminalReason::Unanchored),
            SolveOutcome::Disconnected => Some(TerminalReason::Disconnected),
            SolveOutcome::UnsupportedCoverage { .. } => Some(TerminalReason::UnsupportedCoverage),
            SolveOutcome::VersionMismatch { .. } => Some(TerminalReason::VersionMismatch),
            SolveOutcome::DeadlineExpired => Some(TerminalReason::DeadlineExpired),
            SolveOutcome::Oversized { .. } => Some(TerminalReason::Internal),
            SolveOutcome::Internal { .. } => Some(TerminalReason::Internal),
        }
    }
}

/// One matcher's answer to one job, addressed to the vehicle's result
/// partition.
///
/// `job` and `identity` are copied off the [`SolveJob`] so the reader can
/// [`verify`](SolveResult::verify) the answer against the job it dispatched
/// without touching any store.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct SolveResult<E: Entry> {
    /// The job this answers; equals `identity.job_id()` and is the stream's
    /// `Nats-Msg-Id`.
    pub job: JobId,
    /// The job's full context, echoed so the owner validates without a lookup.
    pub identity: JobIdentity,
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
            outcome,
            solved_at_us,
        }
    }

    /// Check that the echoed `job` id is the one its `identity` hashes to. A
    /// mismatch means the envelope was corrupted or crossed with another job,
    /// and the reader must reject it rather than commit against it.
    pub fn verify(&self) -> Result<(), ResultError> {
        let computed = self.identity.job_id();
        if self.job == computed {
            Ok(())
        } else {
            Err(ResultError::IdMismatch {
                claimed: self.job,
                computed,
            })
        }
    }

    /// The broker dedup key for the result stream: the job id hex. A retried
    /// publish carries the same key, so the broker collapses the duplicate.
    pub fn msg_id(&self) -> String {
        self.job.to_string()
    }

    /// The result subject's partition: the vehicle's fleet-wide partition.
    /// The reader validates that this matches the subject it received on.
    pub fn partition(&self) -> u16 {
        partition_of(self.identity.vehicle_id) as u16
    }
}

impl<E: Entry + serde::de::DeserializeOwned> SolveResult<E> {
    /// Decode from the wire and immediately [`verify`](Self::verify). Fails
    /// with [`ResultError::Decode`] on malformed bytes, or
    /// [`ResultError::IdMismatch`] when the decoded id does not match its
    /// identity.
    ///
    /// [`Entry`] guarantees `Serialize` but not deserialisation, so this
    /// method carries the extra `DeserializeOwned` bound the [`Wire`] decode
    /// it delegates to needs.
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
    /// The echoed `job` id is not the id its `identity` hashes to.
    #[error("result job id {claimed} does not match its identity's job id {computed}")]
    IdMismatch { claimed: JobId, computed: JobId },
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

    /// A representative job identity for `vehicle`, with a committed base so
    /// every identity field participates in the id hash.
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

    /// A job whose `id` matches its identity, ready to answer.
    fn sample_job(identity: JobIdentity) -> SolveJob<MockEntryId> {
        SolveJob {
            id: identity.job_id(),
            identity,
            lane: Lane::DEFAULT,
            deadline_us: 1_726_000_000_000_000,
            context: Continuation::Restart { fresh: Vec::new() },
        }
    }

    /// One representative of every outcome variant. The match in
    /// [`expectations`] has no wildcard, so adding a variant fails to compile
    /// until it is listed here and there.
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
            SolveOutcome::DeadlineExpired,
            SolveOutcome::Oversized {
                bytes: 5 << 20,
                limit: 4 << 20,
            },
            SolveOutcome::Internal {
                reason: "solver panicked".to_owned(),
            },
        ]
    }

    /// The expected classification of one outcome. Exhaustive: a new variant
    /// forces a new arm here, keeping the tables complete.
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
            SolveOutcome::DeadlineExpired => (
                "deadline_expired",
                false,
                false,
                true,
                Some(TerminalReason::DeadlineExpired),
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
        // Every kind label is distinct, so the set size equals the count.
        let mut kinds = std::collections::HashSet::new();
        for outcome in every_outcome() {
            let (kind, success, nominal, terminal, reason) = expectations(&outcome);
            assert_eq!(outcome.kind(), kind, "kind for {kind}");
            assert_eq!(outcome.is_success(), success, "is_success for {kind}");
            assert_eq!(outcome.is_nominal(), nominal, "is_nominal for {kind}");
            assert_eq!(outcome.is_terminal(), terminal, "is_terminal for {kind}");
            // Compare via Debug so the table does not depend on whether
            // `TerminalReason` (owned by T04) derives `PartialEq`.
            assert_eq!(
                format!("{:?}", outcome.terminal_reason()),
                format!("{reason:?}"),
                "terminal_reason for {kind}"
            );
            // Terminal is exactly the negation of success, and success is
            // never nominal.
            assert_eq!(outcome.is_terminal(), !outcome.is_success());
            assert!(!(outcome.is_success() && outcome.is_nominal()));
            kinds.insert(kind);
        }
        assert_eq!(kinds.len(), 8, "every variant has a distinct kind label");
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

        // `new` lifts the id and identity straight off the job.
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
        // Re-encoding the decoded value reproduces the exact bytes.
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
        // A deliberately wrong id: the identity hashes to something else.
        let forged = SolveResult::<MockEntryId> {
            job: JobId(0),
            identity: identity.clone(),
            outcome: SolveOutcome::Unanchored,
            solved_at_us: 0,
        };
        // `ResultError` carries an `anyhow::Error` and so is not `PartialEq`;
        // match on the variant instead of comparing whole values.
        match forged.verify() {
            Err(ResultError::IdMismatch { claimed, computed }) => {
                assert_eq!(claimed, JobId(0));
                assert_eq!(computed, identity.job_id());
            }
            other => panic!("expected IdMismatch, got {other:?}"),
        }

        // Decoding the forged bytes rejects them for the same reason.
        let bytes = forged.encode().expect("encode");
        match SolveResult::<MockEntryId>::decode_verified(&bytes) {
            Err(ResultError::IdMismatch { claimed, .. }) => assert_eq!(claimed, JobId(0)),
            other => panic!("expected IdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn decode_verified_rejects_malformed_bytes() {
        // Truncated postcard is not a decodable result.
        let err = SolveResult::<MockEntryId>::decode_verified(&[0xff]);
        assert!(matches!(err, Err(ResultError::Decode(_))), "got {err:?}");
    }

    #[test]
    fn partition_agrees_with_partition_of() {
        for vehicle in [1u64, 0xdead_beef, 0x1234_5678_9abc_def0, u64::MAX] {
            let identity = sample_identity(vehicle);
            let result = SolveResult::<MockEntryId> {
                job: identity.job_id(),
                identity,
                outcome: SolveOutcome::Unanchored,
                solved_at_us: 0,
            };
            let expected = partition::partition_of(VehicleId(vehicle)) as u16;
            assert_eq!(result.partition(), expected);
            assert!(result.partition() < partition::PARTITIONS as u16);
        }
    }

    #[test]
    fn msg_id_is_the_job_hex() {
        let identity = sample_identity(42);
        let job_id = identity.job_id();
        let result = SolveResult::<MockEntryId> {
            job: job_id,
            identity,
            outcome: SolveOutcome::Unanchored,
            solved_at_us: 0,
        };
        assert_eq!(result.msg_id(), job_id.to_string());
        assert_eq!(result.msg_id().len(), 32);
    }
}
