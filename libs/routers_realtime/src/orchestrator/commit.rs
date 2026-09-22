//! Orchestrator: the commit coordinator.
//!
//! Turns a worker's [`Decision`] into durable, crash-safe history. [`plan`] is
//! pure: it computes the ordered [`CommittedOutput`]s and the next
//! [`VehicleCheckpoint`] from the previous checkpoint. [`Committer`] drives the
//! prepare → publish → promote sequence; every publish is idempotent on the
//! output's [`OutputId`], so [`finish_prepared`](Committer::finish_prepared) can
//! re-drive a commit a crash left half-done.

use core::marker::PhantomData;
use core::time::Duration;
use std::collections::HashSet;

use async_nats::HeaderMap;
use thiserror::Error;

use routers_network::Entry;
use routers_transition::matcher::Trip;

use crate::bus::adapter::{PublishError, PublishOutcome, Publisher};
use crate::bus::{Wire, outbound};
use crate::event::MatchedDiff;
use crate::event::VehicleId;
use crate::protocol::ids::headers::stamp_schema;
use crate::protocol::ids::{
    GraphVersion, JobId, ObservationId, OutputId, RegionId, Revision, SCHEMA_VERSION, SegmentId,
};
use crate::protocol::job::JobIdentity;
use crate::protocol::output::{CommittedOutput, OutputKind, ResetReason, TerminalReason, is_final};
use crate::protocol::result::{SolveOutcome, SolveResult};
use crate::store::checkpoint::{
    CheckpointStore, CommitPhase, PrepareOutcome, PreparedCommit, VehicleCheckpoint,
};
use crate::topology::output::output_subject;

/// The ceiling a doubling publish backoff may reach.
const BACKOFF_CAP: Duration = Duration::from_secs(2);

/// The decision a worker has reached for one observation — the pure input to
/// [`plan`].
// `Solved` is the large variant, but a `Decision` is built immediately before
// `plan` consumes it by value, so boxing would only add a hot-path allocation.
#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug)]
pub enum Decision<E: Entry> {
    /// The solve produced an emission. `reset` is `Some` when this match opens
    /// a new segment (a reset output is emitted first), `None` to continue.
    Solved {
        /// The job that produced the solved outcome.
        job: JobId,
        /// The job's identity (the vehicle is read from it).
        identity: JobIdentity,
        /// The authoritative matched-history change.
        diff: MatchedDiff<E>,
        /// The resumable matcher state after applying this solve.
        trip: Trip<E>,
        /// The event-time watermark through which the match has converged.
        converged_through: Option<i64>,
        /// The reason a new segment was opened, or `None` to continue.
        reset: Option<ResetReason>,
        /// The segment this match belongs to (a new one when `reset` is set).
        segment: SegmentId,
    },
    /// The job ended without a match; `closes_segment` records whether the
    /// vehicle's segment ends here.
    Terminal {
        /// The job that produced the terminal outcome.
        job: JobId,
        /// The job's identity (the vehicle is read from it).
        identity: JobIdentity,
        /// Why the job produced no match.
        reason: TerminalReason,
        /// Whether this terminal closes the vehicle's segment.
        closes_segment: bool,
        /// The segment the terminal output is stamped with.
        segment: SegmentId,
    },
    /// Continuity was broken with no accompanying match: a new segment begins
    /// and the retained trip is dropped.
    Reset {
        /// The job whose commit opens the new segment.
        job: JobId,
        /// The job's identity (the vehicle is read from it).
        identity: JobIdentity,
        /// Why continuity was broken.
        reason: ResetReason,
        /// The segment now opened.
        new_segment: SegmentId,
    },
}

impl<E: Entry> Decision<E> {
    /// Convert a validated matcher result into a commit decision.
    ///
    /// Matching the wire outcome here keeps a terminal outcome out of the
    /// [`Solved`](Self::Solved) variant, so [`plan`] has no invalid outcome
    /// combination to defend against.
    #[must_use]
    pub fn from_result(
        result: SolveResult<E>,
        reset: Option<ResetReason>,
        segment: SegmentId,
    ) -> Self {
        let SolveResult {
            job,
            identity,
            outcome,
            ..
        } = result;
        match outcome {
            SolveOutcome::Solved {
                diff,
                trip,
                converged_through,
            } => Self::Solved {
                job,
                identity,
                diff,
                trip,
                converged_through,
                reset,
                segment,
            },
            SolveOutcome::Unanchored => {
                Self::terminal(job, identity, TerminalReason::Unanchored, segment)
            }
            SolveOutcome::Disconnected => {
                Self::terminal(job, identity, TerminalReason::Disconnected, segment)
            }
            SolveOutcome::UnsupportedCoverage { .. } => {
                Self::terminal(job, identity, TerminalReason::UnsupportedCoverage, segment)
            }
            SolveOutcome::VersionMismatch { .. } => {
                Self::terminal(job, identity, TerminalReason::VersionMismatch, segment)
            }
            SolveOutcome::Oversized { .. } | SolveOutcome::Internal { .. } => {
                Self::terminal(job, identity, TerminalReason::Internal, segment)
            }
        }
    }

    fn terminal(
        job: JobId,
        identity: JobIdentity,
        reason: TerminalReason,
        segment: SegmentId,
    ) -> Self {
        Self::Terminal {
            job,
            identity,
            reason,
            closes_segment: false,
            segment,
        }
    }
}

/// The output of [`plan`]: the committed outputs and next checkpoint, before
/// either is made durable.
#[derive(Clone, Debug)]
pub struct Plan<E: Entry> {
    /// The committed outputs to publish, in publish order.
    pub outputs: Vec<CommittedOutput<E>>,
    /// The checkpoint to install once the outputs are durable.
    pub next: VehicleCheckpoint<E>,
    /// The finality watermark `next` carries, mirrored so the worker need not
    /// decode `next`.
    pub finalized_through: Option<i64>,
    /// How many diff layers were dropped as already-finalised history
    /// (observability only).
    pub stripped_final_layers: usize,
}

/// The later of two optional watermarks; `None` is unset and never lowers a
/// real watermark.
fn later(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (some, None) | (None, some) => some,
    }
}

/// Compute the committed history and next checkpoint for one decision.
///
/// Pure: a function of `prev`, `decision`, and the identifying context.
///
/// Every output and `next` carry [`Revision::from`]`(observation)`. Outputs are
/// ordered `[Reset?] → [Retraction?] → Matched | Terminal | Reset` with
/// ascending [`CommittedOutput::new_indexed`] indices, so a retried commit
/// republishes byte-identical outputs the broker deduplicates.
pub fn plan<E: Entry>(
    prev: Option<&VehicleCheckpoint<E>>,
    decision: Decision<E>,
    observation: ObservationId,
    region: &RegionId,
    graph: &GraphVersion,
    routing_version: u64,
) -> Plan<E> {
    let revision = Revision::from(observation);

    match decision {
        Decision::Solved {
            job,
            identity,
            diff,
            trip,
            converged_through,
            reset,
            segment,
        } => {
            let vehicle = identity.vehicle_id;

            // A reset opens a new segment: its finality base is `None`.
            let segment_changed = reset.is_some();
            let base_finalized = if segment_changed {
                None
            } else {
                prev.and_then(|c| c.finalized_through)
            };

            // Strip layers at or below the base watermark; settled history is never re-emitted.
            let mut stripped_final_layers = 0usize;
            let mut kept = Vec::with_capacity(diff.layers.len());
            for layer in diff.layers {
                if is_final(layer.timestamp, base_finalized) {
                    stripped_final_layers += 1;
                } else {
                    kept.push(layer);
                }
            }

            // Retractions: prior non-final origins the fresh diff no longer covers.
            let retractions: Vec<i64> = match (segment_changed, prev) {
                (false, Some(previous)) => {
                    let kept_ts: HashSet<i64> = kept.iter().map(|l| l.timestamp).collect();
                    previous
                        .trip
                        .origins()
                        .iter()
                        .map(|origin| origin.timestamp)
                        .filter(|&ts| !is_final(ts, previous.finalized_through))
                        .filter(|ts| !kept_ts.contains(ts))
                        .collect()
                }
                _ => Vec::new(),
            };

            let finalized_through = later(base_finalized, converged_through);

            let mut outputs = Vec::new();
            let mut index = 0u8;
            if let Some(reason) = reset {
                outputs.push(CommittedOutput::new_indexed(
                    job,
                    index,
                    vehicle,
                    observation,
                    revision,
                    segment,
                    OutputKind::Reset {
                        reason,
                        new_segment: segment,
                    },
                ));
                index += 1;
            }
            if !retractions.is_empty() {
                outputs.push(CommittedOutput::new_indexed(
                    job,
                    index,
                    vehicle,
                    observation,
                    revision,
                    segment,
                    OutputKind::Retraction {
                        timestamps: retractions,
                    },
                ));
                index += 1;
            }
            let matched_diff = MatchedDiff {
                revision: diff.revision,
                downgraded: diff.downgraded,
                layers: kept,
            };
            outputs.push(CommittedOutput::new_indexed(
                job,
                index,
                vehicle,
                observation,
                revision,
                segment,
                OutputKind::Matched {
                    diff: matched_diff,
                    finalized_through,
                },
            ));

            let next = VehicleCheckpoint {
                trip,
                last_input: observation,
                revision,
                segment,
                finalized_through,
                graph: graph.clone(),
                schema: SCHEMA_VERSION,
                region: region.clone(),
                routing_version,
            };

            Plan {
                outputs,
                next,
                finalized_through,
                stripped_final_layers,
            }
        }

        Decision::Terminal {
            job,
            identity,
            reason,
            closes_segment,
            segment,
        } => {
            let vehicle = identity.vehicle_id;
            // A closing terminal drops the trip and resets the watermark but
            // keeps the segment id; the next observation opens the fresh segment.
            let (trip, finalized_through) = if closes_segment {
                (Trip::new(), None)
            } else {
                match prev {
                    Some(previous) => (previous.trip.clone(), previous.finalized_through),
                    None => (Trip::new(), None),
                }
            };

            let output = CommittedOutput::new_indexed(
                job,
                0,
                vehicle,
                observation,
                revision,
                segment,
                OutputKind::Terminal {
                    reason,
                    closes_segment,
                },
            );

            let next = VehicleCheckpoint {
                trip,
                last_input: observation,
                revision,
                segment,
                finalized_through,
                graph: graph.clone(),
                schema: SCHEMA_VERSION,
                region: region.clone(),
                routing_version,
            };

            Plan {
                outputs: vec![output],
                next,
                finalized_through,
                stripped_final_layers: 0,
            }
        }

        Decision::Reset {
            job,
            identity,
            reason,
            new_segment,
        } => {
            let vehicle = identity.vehicle_id;
            let output = CommittedOutput::new_indexed(
                job,
                0,
                vehicle,
                observation,
                revision,
                new_segment,
                OutputKind::Reset {
                    reason,
                    new_segment,
                },
            );

            let next = VehicleCheckpoint {
                trip: Trip::new(),
                last_input: observation,
                revision,
                segment: new_segment,
                finalized_through: None,
                graph: graph.clone(),
                schema: SCHEMA_VERSION,
                region: region.clone(),
                routing_version,
            };

            Plan {
                outputs: vec![output],
                next,
                finalized_through: None,
                stripped_final_layers: 0,
            }
        }
    }
}

/// How the committer retries an ambiguous publish before leaving the prepared
/// record for recovery.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitConfig {
    /// Maximum publish attempts per output; counts the first attempt, so `1`
    /// means try once and never retry.
    pub publish_attempts: u32,
    /// The first inter-attempt pause; it doubles after each ambiguous attempt,
    /// capped at a fixed two-second maximum.
    pub backoff: Duration,
}

impl Default for CommitConfig {
    fn default() -> Self {
        Self {
            publish_attempts: 5,
            backoff: Duration::from_millis(100),
        }
    }
}

/// The record of a completed commit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Committed {
    /// The raw observation this commit made durable.
    pub raw: ObservationId,
    /// How many outputs were published.
    pub outputs: usize,
    /// `true` when this completion re-drove an already-published record (crash
    /// recovery).
    pub republished: bool,
    /// Encoded output bytes published by this commit.
    pub bytes: usize,
}

/// Why a commit could not complete. In every failure the prepared record is
/// left consistent, so recovery can always re-drive.
#[derive(Debug, Error)]
pub enum CommitError<SE> {
    /// The stored checkpoint no longer matches the expected base; a concurrent
    /// decision won and this commit must be abandoned.
    #[error("commit conflicted with a concurrent decision (stored revision {actual:?})")]
    Conflict {
        /// The revision the store actually holds, or `None` if none.
        actual: Option<Revision>,
    },
    /// The vehicle is already mid-commit on a different output; this one must
    /// wait for that to drain.
    #[error("vehicle is mid-commit on a different output ({pending})")]
    Busy {
        /// The output of the commit already in flight.
        pending: OutputId,
    },
    /// A committed output could not be serialised.
    #[error("could not serialise a committed output")]
    Encode(#[source] anyhow::Error),
    /// The staged output bytes could not be decoded back into the ordered list.
    #[error("could not deserialise the staged commit outputs")]
    Decode(#[source] anyhow::Error),
    /// An output could not be published; the prepared record survives for
    /// recovery.
    #[error("could not publish a committed output to the broker")]
    Publish(#[source] anyhow::Error),
    /// The checkpoint store rejected an operation.
    #[error("the checkpoint store rejected the commit")]
    Store(#[source] SE),
}

/// The commit coordinator: stages a [`Plan`], publishes its outputs, and
/// promotes the next checkpoint — crash-safely and idempotently.
#[derive(Clone, Debug)]
pub struct Committer<E, S, P> {
    store: S,
    publisher: P,
    cfg: CommitConfig,
    _marker: PhantomData<fn() -> E>,
}

impl<E, S, P> Committer<E, S, P>
where
    // `CommittedOutput<E>: Wire` needs `E: DeserializeOwned`; `Entry` alone
    // guarantees only `Serialize`.
    E: Entry + serde::de::DeserializeOwned,
    S: CheckpointStore,
    P: Publisher<CommittedOutput<E>>,
{
    /// Wrap `store` and `publisher` with the retry policy `cfg`.
    #[must_use]
    pub fn new(store: S, publisher: P, cfg: CommitConfig) -> Self {
        Self {
            store,
            publisher,
            cfg,
            _marker: PhantomData,
        }
    }

    /// Stage `plan` and drive it to completion.
    ///
    /// A [`PrepareOutcome::Busy`] or [`PrepareOutcome::Conflict`] ends the
    /// attempt; `Prepared` and `AlreadyPrepared` both proceed (the latter a
    /// safe retry of an identical commit).
    pub async fn commit(
        &self,
        vehicle: VehicleId,
        partition: u16,
        plan: Plan<E>,
        expected_base: Option<Revision>,
        raw: ObservationId,
    ) -> Result<Committed, CommitError<S::Error>> {
        let subject = output_subject(u64::from(partition));
        let mut entries: Vec<(String, String, Vec<u8>)> = Vec::with_capacity(plan.outputs.len());
        for output in &plan.outputs {
            let bytes = output.encode().map_err(CommitError::Encode)?;
            entries.push((subject.clone(), output.msg_id(), bytes));
        }
        let output_bytes =
            postcard::to_allocvec(&entries).map_err(|e| CommitError::Encode(e.into()))?;

        // The record's identity is the first output's id, keyed by every store op.
        let record_output = plan.outputs.first().map(|o| o.id).ok_or_else(|| {
            CommitError::Encode(anyhow::anyhow!("a plan must produce at least one output"))
        })?;
        let next_checkpoint = plan
            .next
            .encode()
            .map_err(|e| CommitError::Encode(e.into()))?;

        let prepared = PreparedCommit {
            output: record_output,
            output_subject: subject,
            output_bytes,
            next_checkpoint,
            next_revision: plan.next.revision,
            next_segment: plan.next.segment,
            expected_base,
            phase: CommitPhase::Prepared,
            raw,
        };

        match self
            .store
            .prepare(vehicle, partition, prepared.clone())
            .await
            .map_err(CommitError::Store)?
        {
            PrepareOutcome::Prepared | PrepareOutcome::AlreadyPrepared => {}
            PrepareOutcome::Busy { pending } => return Err(CommitError::Busy { pending }),
            PrepareOutcome::Conflict { actual } => return Err(CommitError::Conflict { actual }),
        }

        self.finish_prepared(vehicle, partition, prepared).await
    }

    /// Complete a staged commit: (re)publish every stored output, mark the
    /// publication durable, then promote the checkpoint.
    ///
    /// Idempotent, so it re-drives a crash between publish and promote: each
    /// output carries its own dedup key. A failed or exhausted publish returns
    /// [`CommitError::Publish`] and leaves the prepared record for recovery.
    pub async fn finish_prepared(
        &self,
        vehicle: VehicleId,
        partition: u16,
        prepared: PreparedCommit,
    ) -> Result<Committed, CommitError<S::Error>> {
        let entries: Vec<(String, String, Vec<u8>)> = postcard::from_bytes(&prepared.output_bytes)
            .map_err(|e| CommitError::Decode(e.into()))?;

        // An already-published record means a crash between publish and promote.
        let republished = prepared.is_published();

        let mut headers = outbound();
        stamp_schema(&mut headers);

        for (subject, msg_id, bytes) in &entries {
            self.publish_output(subject, msg_id, &headers, bytes)
                .await?;
        }

        self.store
            .finish_published(vehicle, partition, prepared.output)
            .await
            .map_err(CommitError::Store)?;

        Ok(Committed {
            raw: prepared.raw,
            outputs: entries.len(),
            republished,
            bytes: entries.iter().map(|(_, _, bytes)| bytes.len()).sum(),
        })
    }

    /// Publish one output's bytes, retrying an ambiguous send with identical
    /// bytes up to [`CommitConfig::publish_attempts`]; exhaustion or a definite
    /// failure is [`CommitError::Publish`].
    async fn publish_output(
        &self,
        subject: &str,
        msg_id: &str,
        headers: &HeaderMap,
        bytes: &[u8],
    ) -> Result<(), CommitError<S::Error>> {
        let mut backoff = self.cfg.backoff;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self
                .publisher
                .publish_bytes(subject, msg_id, headers.clone(), bytes)
                .await
            {
                Ok(PublishOutcome::Acked { .. }) => return Ok(()),
                Err(PublishError::Ambiguous(_)) if attempt < self.cfg.publish_attempts => {
                    tokio::time::sleep(backoff).await;
                    backoff = backoff.saturating_mul(2).min(BACKOFF_CAP);
                }
                Err(PublishError::Ambiguous(error) | PublishError::Failed(error)) => {
                    return Err(CommitError::Publish(error));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use geo::Point;
    use routers_network::mock::{MockEntryId, MockNetwork, MockNetworkBuilder};
    use routers_network::{DirectionAwareEdgeId, Edge};
    use routers_transition::Matcher;
    use routers_transition::costing::{
        CostingStrategies, DefaultEmissionCost, DefaultTransitionCost,
    };
    use routers_transition::layer::generation::StandardGenerator;
    use routers_transition::matcher::{Continuation, Origin, Trip};
    use routers_transition::weigh::AllCompute;

    use super::*;
    use crate::bus::memory::{MemoryBus, MemoryPublisher};
    use crate::event::{MatchedDiff, MatchedLayer};
    use crate::protocol::ids::Lane;
    use crate::protocol::job::SolveJob;
    use crate::store::checkpoint::{MemoryCheckpointStore, Op};

    type E = MockEntryId;
    type Costing = CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, MockEntryId>;

    fn region() -> RegionId {
        RegionId::new("r1").unwrap()
    }

    fn graph() -> GraphVersion {
        GraphVersion::new("g1").unwrap()
    }

    fn obs(seq: u64) -> ObservationId {
        ObservationId {
            partition: 7,
            sequence: seq,
        }
    }

    fn identity(vehicle: u64, seq: u64) -> JobIdentity {
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(vehicle),
            observation: obs(seq),
            base: None,
            graph: graph(),
            region: region(),
        }
    }

    fn layer(ts: i64) -> MatchedLayer<E> {
        MatchedLayer {
            timestamp: ts,
            edge: Edge {
                source: MockEntryId(1),
                target: MockEntryId(2),
                weight: 1,
                id: DirectionAwareEdgeId::new(MockEntryId(3)),
            },
            position: Point::new(0.0, 0.0),
            path: Vec::new(),
        }
    }

    fn diff(revision: u64, timestamps: &[i64]) -> MatchedDiff<E> {
        MatchedDiff {
            revision,
            downgraded: false,
            layers: timestamps.iter().map(|&ts| layer(ts)).collect(),
        }
    }

    fn solved(
        vehicle: u64,
        seq: u64,
        diff: MatchedDiff<E>,
        trip: Trip<E>,
        converged: Option<i64>,
    ) -> SolveResult<E> {
        let job = SolveJob::<E>::new(
            identity(vehicle, seq),
            Lane::DEFAULT,
            1_000,
            Continuation::Restart { fresh: Vec::new() },
        );
        SolveResult::new(
            &job,
            SolveOutcome::Solved {
                diff,
                trip,
                converged_through: converged,
            },
            0,
        )
    }

    fn checkpoint(
        trip: Trip<E>,
        revision: u64,
        segment: u64,
        finalized: Option<i64>,
    ) -> VehicleCheckpoint<E> {
        VehicleCheckpoint {
            trip,
            last_input: obs(revision),
            revision: Revision(revision),
            segment: SegmentId(segment),
            finalized_through: finalized,
            graph: graph(),
            schema: SCHEMA_VERSION,
            region: region(),
            routing_version: 1,
        }
    }

    fn bent_road() -> MockNetwork {
        MockNetworkBuilder::new()
            .node(1, geo::point!(x: -118.15, y: 34.15))
            .node(2, geo::point!(x: -118.16, y: 34.15))
            .node(3, geo::point!(x: -118.17, y: 34.15))
            .node(4, geo::point!(x: -118.17, y: 34.14))
            .node(5, geo::point!(x: -118.18, y: 34.14))
            .edge(1, 2)
            .edge(2, 3)
            .edge(3, 4)
            .edge(4, 5)
            .build()
    }

    const TRACE_START_US: i64 = 1_775_000_000_000_000;

    fn trace_ts(i: i64) -> i64 {
        TRACE_START_US + i * 5_000_000
    }

    fn trace_origins() -> Vec<Origin> {
        [
            geo::point!(x: -118.151, y: 34.1503),
            geo::point!(x: -118.155, y: 34.1503),
            geo::point!(x: -118.165, y: 34.1503),
            geo::point!(x: -118.170, y: 34.1490),
            geo::point!(x: -118.172, y: 34.1403),
            geo::point!(x: -118.179, y: 34.1403),
        ]
        .into_iter()
        .enumerate()
        .map(|(i, pt)| Origin::new(pt, trace_ts(i as i64)))
        .collect()
    }

    /// Build a non-empty [`Trip`] by pushing `origins` through a matcher (the
    /// only way to mint one).
    fn trip_with(origins: &[Origin]) -> Trip<E> {
        let net = bent_road();
        let costing = Costing::default();
        let generator = StandardGenerator::new(&net, &costing.emission);
        let m = Matcher::new(&net, &costing, generator, AllCompute::default(), &());
        let mut trip = m.begin();
        for &origin in origins {
            m.push(&mut trip, origin).expect("observation must anchor");
        }
        trip
    }

    fn kinds(plan: &Plan<E>) -> Vec<&'static str> {
        plan.outputs.iter().map(|o| o.kind.kind()).collect()
    }

    #[test]
    fn result_conversion_classifies_every_terminal_outcome() {
        let cases: Vec<(SolveOutcome<E>, TerminalReason)> = vec![
            (SolveOutcome::Unanchored, TerminalReason::Unanchored),
            (SolveOutcome::Disconnected, TerminalReason::Disconnected),
            (
                SolveOutcome::UnsupportedCoverage {
                    cell: "r3gx".to_owned(),
                },
                TerminalReason::UnsupportedCoverage,
            ),
            (
                SolveOutcome::VersionMismatch {
                    expected: graph(),
                    got: GraphVersion::new("g2").unwrap(),
                },
                TerminalReason::VersionMismatch,
            ),
            (
                SolveOutcome::Oversized {
                    bytes: 11,
                    limit: 10,
                },
                TerminalReason::Internal,
            ),
            (
                SolveOutcome::Internal {
                    reason: "matcher fault".to_owned(),
                },
                TerminalReason::Internal,
            ),
        ];

        for (index, (outcome, expected_reason)) in cases.into_iter().enumerate() {
            let seq = 100 + index as u64;
            let job = SolveJob::<E>::new(
                identity(1, seq),
                Lane::DEFAULT,
                1_000,
                Continuation::Restart { fresh: Vec::new() },
            );
            let expected_job = job.id;
            let decision = Decision::from_result(
                SolveResult::new(&job, outcome, 0),
                Some(ResetReason::Gap),
                SegmentId(7),
            );

            match decision {
                Decision::Terminal {
                    job,
                    identity,
                    reason,
                    closes_segment,
                    segment,
                } => {
                    assert_eq!(job, expected_job);
                    assert_eq!(identity.observation, obs(seq));
                    assert_eq!(reason, expected_reason);
                    assert!(!closes_segment);
                    assert_eq!(segment, SegmentId(7));
                }
                Decision::Solved { .. } | Decision::Reset { .. } => {
                    panic!("a terminal solve outcome must become a terminal decision")
                }
            }
        }
    }

    #[test]
    fn solved_finalization_is_monotone_within_a_segment() {
        for (converged, expected) in [
            (Some(50), Some(100)),
            (Some(200), Some(200)),
            (None, Some(100)),
        ] {
            let prev = checkpoint(Trip::new(), 5, 3, Some(100));
            let plan = plan(
                Some(&prev),
                Decision::from_result(
                    solved(1, 200, diff(200, &[150]), Trip::new(), converged),
                    None,
                    SegmentId(3),
                ),
                obs(200),
                &region(),
                &graph(),
                1,
            );
            assert_eq!(plan.finalized_through, expected, "converged {converged:?}");
            assert_eq!(plan.next.finalized_through, expected);
            assert_eq!(plan.next.revision, Revision(200));
        }
    }

    #[test]
    fn solved_strips_layers_at_or_below_the_finality_watermark() {
        let prev = checkpoint(Trip::new(), 5, 3, Some(100));
        let plan = plan(
            Some(&prev),
            Decision::from_result(
                solved(1, 200, diff(200, &[50, 150]), Trip::new(), Some(150)),
                None,
                SegmentId(3),
            ),
            obs(200),
            &region(),
            &graph(),
            1,
        );

        assert_eq!(plan.stripped_final_layers, 1);
        assert_eq!(kinds(&plan), vec!["matched"]);
        match &plan.outputs[0].kind {
            OutputKind::Matched { diff, .. } => {
                let tss: Vec<i64> = diff.layers.iter().map(|l| l.timestamp).collect();
                assert_eq!(tss, vec![150]);
            }
            other => panic!("expected Matched, got {}", other.kind()),
        }
    }

    #[test]
    fn solved_computes_retractions_for_dropped_non_final_origins() {
        // Watermark finalises trace_ts(0..=2); the diff keeps trace_ts(3), so
        // trace_ts(4) and trace_ts(5) are retracted.
        let prev = checkpoint(trip_with(&trace_origins()), 5, 3, Some(trace_ts(2)));
        let plan = plan(
            Some(&prev),
            Decision::from_result(
                solved(
                    1,
                    200,
                    diff(200, &[trace_ts(0), trace_ts(3)]),
                    Trip::new(),
                    Some(trace_ts(3)),
                ),
                None,
                SegmentId(3),
            ),
            obs(200),
            &region(),
            &graph(),
            1,
        );

        assert_eq!(plan.stripped_final_layers, 1, "trace_ts(0) is finalised");
        assert_eq!(kinds(&plan), vec!["retraction", "matched"]);
        match &plan.outputs[0].kind {
            OutputKind::Retraction { timestamps } => {
                assert_eq!(timestamps, &vec![trace_ts(4), trace_ts(5)]);
            }
            other => panic!("expected Retraction, got {}", other.kind()),
        }
        assert_eq!(plan.outputs[0].revision, Revision(200));
        assert_eq!(plan.outputs[1].revision, Revision(200));
    }

    #[test]
    fn reset_then_matched_are_two_ordered_indexed_outputs() {
        let prev = checkpoint(trip_with(&trace_origins()), 5, 3, Some(trace_ts(2)));
        let identity = identity(1, 200);
        let job = SolveJob::<E>::new(
            identity,
            Lane::DEFAULT,
            1_000,
            Continuation::Restart { fresh: Vec::new() },
        )
        .id;
        let plan = plan(
            Some(&prev),
            Decision::from_result(
                // A reset abandons the old segment: nothing stripped, no retractions.
                solved(
                    1,
                    200,
                    diff(200, &[trace_ts(0), trace_ts(3)]),
                    Trip::new(),
                    Some(trace_ts(3)),
                ),
                Some(ResetReason::Gap),
                SegmentId(200),
            ),
            obs(200),
            &region(),
            &graph(),
            1,
        );

        assert_eq!(plan.stripped_final_layers, 0);
        assert_eq!(kinds(&plan), vec!["reset", "matched"]);

        assert_eq!(
            plan.outputs[0].id,
            CommittedOutput::<E>::new_indexed(
                job,
                0,
                VehicleId(1),
                obs(200),
                Revision(200),
                SegmentId(200),
                OutputKind::Reset {
                    reason: ResetReason::Gap,
                    new_segment: SegmentId(200),
                },
            )
            .id
        );
        assert_ne!(plan.outputs[0].id, plan.outputs[1].id);

        match &plan.outputs[0].kind {
            OutputKind::Reset {
                reason,
                new_segment,
            } => {
                assert_eq!(*reason, ResetReason::Gap);
                assert_eq!(*new_segment, SegmentId(200));
            }
            other => panic!("expected Reset, got {}", other.kind()),
        }
        assert_eq!(plan.finalized_through, Some(trace_ts(3)));
        assert_eq!(plan.next.segment, SegmentId(200));
    }

    #[test]
    fn terminal_keeps_the_trip_and_closing_terminal_empties_it() {
        let prev = checkpoint(trip_with(&trace_origins()), 5, 3, Some(trace_ts(2)));
        let id = identity(1, 200);

        // Non-closing: the retained trip and watermark survive unchanged.
        let kept = plan(
            Some(&prev),
            Decision::Terminal {
                job: id.local_decision_id(),
                identity: id.clone(),
                reason: TerminalReason::Unanchored,
                closes_segment: false,
                segment: SegmentId(3),
            },
            obs(200),
            &region(),
            &graph(),
            1,
        );
        assert_eq!(kinds(&kept), vec!["terminal"]);
        assert_eq!(kept.next.trip.layers(), prev.trip.layers());
        assert_eq!(kept.next.finalized_through, Some(trace_ts(2)));
        assert_eq!(kept.next.segment, SegmentId(3));
        assert_eq!(kept.next.revision, Revision(200));

        // Closing: the trip is dropped and the watermark reset, segment kept.
        let closed = plan(
            Some(&prev),
            Decision::Terminal {
                job: id.local_decision_id(),
                identity: id,
                reason: TerminalReason::JobExhausted,
                closes_segment: true,
                segment: SegmentId(3),
            },
            obs(201),
            &region(),
            &graph(),
            1,
        );
        assert_eq!(closed.next.trip.layers(), 0);
        assert_eq!(closed.next.finalized_through, None);
        assert_eq!(closed.next.segment, SegmentId(3));
    }

    #[test]
    fn reset_decision_opens_an_empty_segment() {
        let prev = checkpoint(trip_with(&trace_origins()), 5, 3, Some(trace_ts(2)));
        let id = identity(1, 300);
        let plan = plan(
            Some(&prev),
            Decision::Reset {
                job: id.local_decision_id(),
                identity: id,
                reason: ResetReason::StateLost,
                new_segment: SegmentId(300),
            },
            obs(300),
            &region(),
            &graph(),
            1,
        );

        assert_eq!(kinds(&plan), vec!["reset"]);
        assert_eq!(plan.next.trip.layers(), 0);
        assert_eq!(plan.next.segment, SegmentId(300));
        assert_eq!(plan.next.finalized_through, None);
        assert_eq!(plan.next.revision, Revision(300));
    }

    fn committer(
        store: MemoryCheckpointStore,
        bus: &MemoryBus,
    ) -> Committer<E, MemoryCheckpointStore, MemoryPublisher<CommittedOutput<E>>> {
        Committer::new(
            store,
            bus.publisher::<CommittedOutput<E>>(),
            CommitConfig {
                publish_attempts: 4,
                backoff: Duration::ZERO,
            },
        )
    }

    fn terminal_plan(seq: u64) -> Plan<E> {
        let id = identity(1, seq);
        plan(
            None,
            Decision::Terminal {
                job: id.local_decision_id(),
                identity: id,
                reason: TerminalReason::Unanchored,
                closes_segment: false,
                segment: SegmentId(seq),
            },
            obs(seq),
            &region(),
            &graph(),
            1,
        )
    }

    fn reset_matched_plan(seq: u64) -> Plan<E> {
        plan(
            None,
            Decision::from_result(
                solved(
                    1,
                    seq,
                    diff(seq, &[trace_ts(0)]),
                    Trip::new(),
                    Some(trace_ts(0)),
                ),
                Some(ResetReason::Teleport),
                SegmentId(seq),
            ),
            obs(seq),
            &region(),
            &graph(),
            1,
        )
    }

    const SUBJECT: &str = "events.matched.v1.p.7";

    #[tokio::test]
    async fn commit_stores_the_next_checkpoint_and_clears_prepared() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let c = committer(store.clone(), &bus);

        let committed = c
            .commit(VehicleId(1), 7, terminal_plan(100), None, obs(100))
            .await
            .expect("commit");

        assert_eq!(committed.outputs, 1);
        assert_eq!(committed.raw, obs(100));
        assert!(!committed.republished);

        assert_eq!(bus.published(SUBJECT).len(), 1);

        let (checkpoint, pending) = store.load(VehicleId(1)).await.unwrap();
        assert_eq!(checkpoint.unwrap().revision, Revision(100));
        assert!(pending.is_none());
    }

    #[tokio::test]
    async fn commit_publishes_every_output_of_a_multi_output_plan() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let c = committer(store.clone(), &bus);

        let committed = c
            .commit(VehicleId(1), 7, reset_matched_plan(100), None, obs(100))
            .await
            .expect("commit");

        assert_eq!(committed.outputs, 2);
        assert_eq!(bus.published(SUBJECT).len(), 2);
        assert!(store.load(VehicleId(1)).await.unwrap().1.is_none());
    }

    #[tokio::test]
    async fn commit_conflicts_when_the_expected_base_is_wrong() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        let c = committer(store.clone(), &bus);

        c.commit(VehicleId(1), 7, terminal_plan(100), None, obs(100))
            .await
            .expect("seed commit");

        let err = c
            .commit(VehicleId(1), 7, terminal_plan(101), None, obs(101))
            .await
            .expect_err("stale base must conflict");
        assert!(
            matches!(
                err,
                CommitError::Conflict {
                    actual: Some(Revision(100))
                }
            ),
            "got {err:?}"
        );
    }

    #[tokio::test]
    async fn commit_retries_an_ambiguous_publish_to_a_single_stored_message() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        // The ambiguous publish stores the message *and* reports the error, so
        // the retry must deduplicate to one stored copy.
        bus.fail_next_publish(PublishError::Ambiguous(anyhow::anyhow!("ack lost")));
        let c = committer(store.clone(), &bus);

        let committed = c
            .commit(VehicleId(1), 7, terminal_plan(100), None, obs(100))
            .await
            .expect("commit completes after a retry");

        assert_eq!(committed.outputs, 1);
        assert_eq!(bus.published(SUBJECT).len(), 1, "the retry deduplicates");
        assert!(store.load(VehicleId(1)).await.unwrap().0.is_some());
    }

    #[tokio::test]
    async fn a_failed_publish_leaves_the_prepared_record_for_recovery() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        bus.fail_next_publish(PublishError::Failed(anyhow::anyhow!("refused")));
        let c = committer(store.clone(), &bus);

        let err = c
            .commit(VehicleId(1), 7, terminal_plan(100), None, obs(100))
            .await
            .expect_err("a failed publish fails the commit");
        assert!(matches!(err, CommitError::Publish(_)), "got {err:?}");
        assert!(bus.published(SUBJECT).is_empty());

        let (checkpoint, pending) = store.load(VehicleId(1)).await.unwrap();
        assert!(checkpoint.is_none());
        let prepared = pending.expect("the prepared record is retained");

        let committed = c
            .finish_prepared(VehicleId(1), 7, prepared)
            .await
            .expect("recovery completes the commit");
        assert_eq!(committed.outputs, 1);
        assert_eq!(bus.published(SUBJECT).len(), 1);
        assert_eq!(
            store.load(VehicleId(1)).await.unwrap().0.unwrap().revision,
            Revision(100)
        );
    }

    #[tokio::test]
    async fn a_crash_between_publish_and_promote_is_re_driven_idempotently() {
        let store = MemoryCheckpointStore::new();
        let bus = MemoryBus::new();
        // Promotion fails once, after the output is published and marked.
        store.fail_next(Op::Promote);
        let c = committer(store.clone(), &bus);

        let err = c
            .commit(VehicleId(1), 7, terminal_plan(100), None, obs(100))
            .await
            .expect_err("promote fails");
        assert!(matches!(err, CommitError::Store(_)), "got {err:?}");

        assert_eq!(bus.published(SUBJECT).len(), 1);
        let prepared = store
            .load(VehicleId(1))
            .await
            .unwrap()
            .1
            .expect("still staged");
        assert!(prepared.is_published());

        let committed = c
            .finish_prepared(VehicleId(1), 7, prepared)
            .await
            .expect("recovery promotes");
        assert!(
            committed.republished,
            "an already-published record re-drives"
        );
        assert_eq!(
            bus.published(SUBJECT).len(),
            1,
            "the republish deduplicates"
        );
        assert_eq!(
            store.load(VehicleId(1)).await.unwrap().0.unwrap().revision,
            Revision(100)
        );
        assert!(store.load(VehicleId(1)).await.unwrap().1.is_none());
    }
}
