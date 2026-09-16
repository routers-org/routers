//! `CommittedOutput`: the authoritative matched-output message. This is the one
//! message the outside world reads to learn what a vehicle's history is.
//!
//! A single triggering observation may commit several outputs, in order (a reset
//! then the matched layers, or a retraction before a fresh match). Each carries
//! its own [`OutputId`] so the broker dedups them independently while a retried
//! commit republishes byte-identical output.

use serde::{Deserialize, Serialize};

use routers_network::Entry;

use crate::bus::postcard_wire;
use crate::event::{MatchedDiff, VehicleId};
use crate::partition::partition_of;
use crate::protocol::ids::{self, JobId, ObservationId, OutputId, Revision, SegmentId};

/// Why a job produced no match. Terminal outcomes never rewrite finalised
/// layers; whether they close the segment is carried by `Terminal::closes_segment`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TerminalReason {
    /// The job's deadline passed before a result committed.
    DeadlineExpired,
    /// The matcher ran the job to exhaustion without an emission.
    JobExhausted,
    /// The observation fell outside the loaded shard coverage.
    UnsupportedCoverage,
    /// The job was solved against a graph version this owner no longer trusts.
    VersionMismatch,
    /// No layer could anchor to the network (a nominal solve outcome).
    Unanchored,
    /// The network could not bridge the trace (a nominal solve outcome).
    Disconnected,
    /// An unexpected fault; the reason string stays in logs, not on the wire.
    Internal,
}

impl TerminalReason {
    /// A stable snake_case label for bounded metric dimensions.
    pub fn label(&self) -> &'static str {
        match self {
            TerminalReason::DeadlineExpired => "deadline_expired",
            TerminalReason::JobExhausted => "job_exhausted",
            TerminalReason::UnsupportedCoverage => "unsupported_coverage",
            TerminalReason::VersionMismatch => "version_mismatch",
            TerminalReason::Unanchored => "unanchored",
            TerminalReason::Disconnected => "disconnected",
            TerminalReason::Internal => "internal",
        }
    }
}

impl core::fmt::Display for TerminalReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// Why continuity was broken and a new segment opened. Prior finalised layers
/// are never disturbed by a reset.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ResetReason {
    /// The committed checkpoint was missing or undecodable, so state was lost.
    StateLost,
    /// The vehicle jumped further than `jump_distance` between observations.
    Teleport,
    /// The gap since the last committed observation exceeded `gap`.
    Gap,
    /// An operator deliberately severed continuity.
    Operator,
}

impl ResetReason {
    /// A stable snake_case label for bounded metric dimensions.
    pub fn label(&self) -> &'static str {
        match self {
            ResetReason::StateLost => "state_lost",
            ResetReason::Teleport => "teleport",
            ResetReason::Gap => "gap",
            ResetReason::Operator => "operator",
        }
    }
}

impl core::fmt::Display for ResetReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.label())
    }
}

/// What a committed output *says* about a vehicle's history.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub enum OutputKind<E: Entry> {
    /// The authoritative layers for this revision. Layers with
    /// `timestamp <= finalized_through` are final and must never be rewritten;
    /// layers above it may still be refined.
    Matched {
        diff: MatchedDiff<E>,
        finalized_through: Option<i64>,
    },

    /// Layers previously emitted (and not yet final) that no longer exist in
    /// this segment. Consumers drop these timestamps.
    Retraction { timestamps: Vec<i64> },

    /// A job ended without a match. The segment stays open unless
    /// `closes_segment`.
    Terminal {
        reason: TerminalReason,
        closes_segment: bool,
    },

    /// Continuity was lost or deliberately broken; a new segment begins. Prior
    /// finalised layers stay untouched.
    Reset {
        reason: ResetReason,
        new_segment: SegmentId,
    },
}

impl<E: Entry> OutputKind<E> {
    /// A stable, bounded label for metrics and log correlation.
    pub fn kind(&self) -> &'static str {
        match self {
            OutputKind::Matched { .. } => "matched",
            OutputKind::Retraction { .. } => "retraction",
            OutputKind::Terminal { .. } => "terminal",
            OutputKind::Reset { .. } => "reset",
        }
    }
}

/// One committed decision about a vehicle, addressed to the matched-output
/// plane. `revision` orders competing decisions for the same vehicle and
/// `segment` scopes finality so a reset can start afresh.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct CommittedOutput<E: Entry> {
    /// This output's own identity; the broker dedup key on the matched plane.
    pub id: OutputId,
    pub vehicle_id: VehicleId,
    /// The observation whose commit produced this output.
    pub observation: ObservationId,
    /// Orders competing outputs for the vehicle; higher supersedes lower.
    pub revision: Revision,
    /// The continuity generation this output belongs to.
    pub segment: SegmentId,
    pub kind: OutputKind<E>,
}

impl<E: Entry> CommittedOutput<E> {
    /// Build the sole output of a job; equivalent to
    /// [`new_indexed`](Self::new_indexed) at index `0`.
    pub fn new(
        job: JobId,
        vehicle_id: VehicleId,
        observation: ObservationId,
        revision: Revision,
        segment: SegmentId,
        kind: OutputKind<E>,
    ) -> Self {
        Self::new_indexed(job, 0, vehicle_id, observation, revision, segment, kind)
    }

    /// Build the `index`-th output of a job. The identity is
    /// `digest128("routers.output.v1" ∥ job_be_bytes ∥ index)`, so a job's
    /// several ordered outputs each get a distinct, deterministic id.
    pub fn new_indexed(
        job: JobId,
        index: u8,
        vehicle_id: VehicleId,
        observation: ObservationId,
        revision: Revision,
        segment: SegmentId,
        kind: OutputKind<E>,
    ) -> Self {
        let id = OutputId(ids::digest128(&[
            b"routers.output.v1".as_slice(),
            job.0.to_be_bytes().as_slice(),
            [index].as_slice(),
        ]));
        Self {
            id,
            vehicle_id,
            observation,
            revision,
            segment,
            kind,
        }
    }

    /// The broker dedup key (`Nats-Msg-Id`): this output's id as 32 hex chars.
    pub fn msg_id(&self) -> String {
        self.id.to_string()
    }

    /// The matched-plane partition this output belongs to: the vehicle's
    /// partition under the fleet-wide hashing law.
    pub fn partition(&self) -> u16 {
        partition_of(self.vehicle_id) as u16
    }

    /// The subject to publish this output on.
    pub fn subject(&self) -> String {
        crate::topology::output::output_subject(u64::from(self.partition()))
    }
}

postcard_wire!(CommittedOutput<E: Entry>);

/// Whether an `incoming` revision should replace an `existing` one for the same
/// (vehicle, timestamp). Strictly greater wins; equal is a duplicate and lower
/// is stale. `None` means nothing is held yet, so anything supersedes it.
pub fn supersedes(incoming: Revision, existing: Option<Revision>) -> bool {
    match existing {
        None => true,
        Some(current) => incoming.0 > current.0,
    }
}

/// Whether a layer at `timestamp` is final given the segment's
/// `finalized_through` watermark: final iff at or below the watermark. A layer
/// with no watermark (`None`) is never final and may still be refined.
pub fn is_final(timestamp: i64, finalized_through: Option<i64>) -> bool {
    matches!(finalized_through, Some(through) if timestamp <= through)
}

#[cfg(test)]
mod tests {
    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};

    use crate::bus::Wire;
    use crate::event::{MatchedDiff, MatchedLayer};

    use super::*;

    type E = MockEntryId;

    fn sample_diff() -> MatchedDiff<E> {
        MatchedDiff {
            revision: 7,
            downgraded: false,
            layers: vec![MatchedLayer {
                timestamp: 1_700_000_000_000_000,
                edge: Edge {
                    source: MockEntryId(1),
                    target: MockEntryId(2),
                    weight: 42,
                    id: DirectionAwareEdgeId::new(MockEntryId(3)),
                },
                position: Point::new(151.2, -33.8),
                path: vec![Point::new(151.1, -33.7), Point::new(151.15, -33.75)],
            }],
        }
    }

    fn each_kind() -> Vec<OutputKind<E>> {
        vec![
            OutputKind::Matched {
                diff: sample_diff(),
                finalized_through: Some(1_699_999_999_000_000),
            },
            OutputKind::Matched {
                diff: sample_diff(),
                finalized_through: None,
            },
            OutputKind::Retraction {
                timestamps: vec![1, 2, 3],
            },
            OutputKind::Terminal {
                reason: TerminalReason::Unanchored,
                closes_segment: false,
            },
            OutputKind::Reset {
                reason: ResetReason::Gap,
                new_segment: SegmentId(99),
            },
        ]
    }

    fn output_with(kind: OutputKind<E>) -> CommittedOutput<E> {
        CommittedOutput::new(
            JobId(0xdead_beef),
            VehicleId(1),
            ObservationId {
                partition: 485,
                sequence: 42,
            },
            Revision(42),
            SegmentId(7),
            kind,
        )
    }

    #[test]
    fn every_kind_round_trips_on_the_wire() {
        for kind in each_kind() {
            let label = kind.kind();
            let out = output_with(kind);
            let bytes = out.encode().expect("encode");
            let decoded = CommittedOutput::<E>::decode(&bytes).expect("decode");

            // Re-encode compares bytes since the payload types lack `PartialEq`.
            assert_eq!(decoded.encode().expect("re-encode"), bytes, "{label}");
            assert_eq!(decoded.id, out.id, "{label}");
            assert_eq!(decoded.vehicle_id, out.vehicle_id, "{label}");
            assert_eq!(decoded.observation, out.observation, "{label}");
            assert_eq!(decoded.revision, out.revision, "{label}");
            assert_eq!(decoded.segment, out.segment, "{label}");
            assert_eq!(decoded.kind.kind(), label, "{label}");
        }
    }

    #[test]
    fn kind_labels_are_stable() {
        for kind in each_kind() {
            let expected = match kind {
                OutputKind::Matched { .. } => "matched",
                OutputKind::Retraction { .. } => "retraction",
                OutputKind::Terminal { .. } => "terminal",
                OutputKind::Reset { .. } => "reset",
            };
            assert_eq!(kind.kind(), expected);
        }
    }

    #[test]
    fn new_is_indexed_zero_and_indices_are_distinct() {
        let job = JobId(0x1234_5678_9abc_def0);
        let mk = |index: u8| {
            CommittedOutput::<E>::new_indexed(
                job,
                index,
                VehicleId(1),
                ObservationId {
                    partition: 485,
                    sequence: 1,
                },
                Revision(1),
                SegmentId(1),
                OutputKind::Terminal {
                    reason: TerminalReason::JobExhausted,
                    closes_segment: true,
                },
            )
            .id
        };

        assert_eq!(
            output_with(OutputKind::Terminal {
                reason: TerminalReason::JobExhausted,
                closes_segment: true,
            })
            .id,
            CommittedOutput::<E>::new_indexed(
                JobId(0xdead_beef),
                0,
                VehicleId(1),
                ObservationId {
                    partition: 485,
                    sequence: 42,
                },
                Revision(42),
                SegmentId(7),
                OutputKind::Terminal {
                    reason: TerminalReason::JobExhausted,
                    closes_segment: true,
                },
            )
            .id
        );

        let ids: Vec<OutputId> = [0u8, 1, 2, 3, 255].into_iter().map(mk).collect();
        for (i, a) in ids.iter().enumerate() {
            for b in ids.iter().skip(i + 1) {
                assert_ne!(a, b, "indices must not collide");
            }
        }

        assert_eq!(mk(1), mk(1));
    }

    #[test]
    fn msg_id_partition_and_subject() {
        let out = output_with(OutputKind::Retraction { timestamps: vec![] });
        assert_eq!(out.msg_id(), out.id.to_string());
        // VehicleId(1) partitions to 485 under the fleet law (see partition.rs).
        assert_eq!(out.partition(), 485);
        assert_eq!(out.subject(), "events.matched.v1.p.485");
    }

    #[test]
    fn supersedes_table() {
        let cases = [
            (Revision(5), None, true),
            (Revision(5), Some(Revision(4)), true),
            (Revision(5), Some(Revision(5)), false),
            (Revision(5), Some(Revision(6)), false),
            (Revision(0), None, true),
        ];
        for (incoming, existing, expected) in cases {
            assert_eq!(
                supersedes(incoming, existing),
                expected,
                "supersedes({incoming:?}, {existing:?})"
            );
        }
    }

    #[test]
    fn is_final_table() {
        let cases = [
            (10, None, false),
            (10, Some(20), true),
            (20, Some(20), true),
            (21, Some(20), false),
            (i64::MIN, Some(0), true),
            (i64::MAX, Some(0), false),
        ];
        for (timestamp, finalized_through, expected) in cases {
            assert_eq!(
                is_final(timestamp, finalized_through),
                expected,
                "is_final({timestamp}, {finalized_through:?})"
            );
        }
    }

    #[test]
    fn terminal_reason_labels_are_snake_case() {
        let all = [
            TerminalReason::DeadlineExpired,
            TerminalReason::JobExhausted,
            TerminalReason::UnsupportedCoverage,
            TerminalReason::VersionMismatch,
            TerminalReason::Unanchored,
            TerminalReason::Disconnected,
            TerminalReason::Internal,
        ];
        for reason in all {
            let expected = match reason {
                TerminalReason::DeadlineExpired => "deadline_expired",
                TerminalReason::JobExhausted => "job_exhausted",
                TerminalReason::UnsupportedCoverage => "unsupported_coverage",
                TerminalReason::VersionMismatch => "version_mismatch",
                TerminalReason::Unanchored => "unanchored",
                TerminalReason::Disconnected => "disconnected",
                TerminalReason::Internal => "internal",
            };
            assert_eq!(reason.label(), expected);
            assert_eq!(reason.to_string(), expected);
        }
    }

    #[test]
    fn reset_reason_labels_are_snake_case() {
        let all = [
            ResetReason::StateLost,
            ResetReason::Teleport,
            ResetReason::Gap,
            ResetReason::Operator,
        ];
        for reason in all {
            let expected = match reason {
                ResetReason::StateLost => "state_lost",
                ResetReason::Teleport => "teleport",
                ResetReason::Gap => "gap",
                ResetReason::Operator => "operator",
            };
            assert_eq!(reason.label(), expected);
            assert_eq!(reason.to_string(), expected);
        }
    }
}
