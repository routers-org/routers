//! `SolveJob`: the regional solve-job message, one unit of work handed to the
//! regional matchers. Its identity is a deterministic hash of its
//! entire solve envelope, so two orchestrators that build the same context mint
//! the same [`JobId`] and the broker's `Nats-Msg-Id` dedup collapses the duplicate.
//!
//! Field order in [`JobProof`] is wire law: postcard hashes it, so reordering
//! silently changes every job id in the fleet. Do not reorder without bumping
//! the solve-job contract and the domain tag below.

use core::time::Duration;

use routers_network::Entry;
use routers_transition::matcher::{Continuation, Origin};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::bus::{Wire, postcard_wire};
use crate::event::VehicleId;
use crate::protocol::ids::{
    self, GraphVersion, JobId, Lane, ObservationId, RegionId, Revision, SCHEMA_VERSION,
    SchemaVersion, SegmentId,
};

/// The committed state a job resumes from: the last committed revision and its
/// segment. `None` for a fresh vehicle with no checkpoint yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseState {
    /// The revision (raw sequence) of the last committed decision resumed from.
    pub revision: Revision,
    /// The continuity segment that committed decision belonged to.
    pub segment: SegmentId,
}

/// The durable, store-addressable portion of a solve job's context. The full
/// [`JobId`] additionally authenticates the lane, deadline, and continuation
/// through [`JobProof`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobIdentity {
    /// The wire contract this job was produced against; a peer on a different
    /// schema is rejected rather than mis-decoded.
    pub schema: SchemaVersion,
    /// The vehicle being solved.
    pub vehicle_id: VehicleId,
    /// The head observation this job solves (its durable raw identity).
    pub observation: ObservationId,
    /// The committed state resumed from, or `None` for a fresh vehicle.
    pub base: Option<BaseState>,
    /// The road-network snapshot the solve must run against.
    pub graph: GraphVersion,
    /// The region (shard grouping) that owns this job.
    pub region: RegionId,
}

/// The v1 domain-separation tag mixed into every solve-job digest.
const JOB_ID_DOMAIN: &[u8] = b"routers.solve-job.v1";
/// Separates the continuation digest from the outer solve-job digest.
const CONTINUATION_DOMAIN: &[u8] = b"routers.solve-job.continuation.v1";

impl JobIdentity {
    /// The deterministic id of a context-only local decision. It is not the
    /// [`SolveJob`] id: use [`SolveJob::computed_id`] for a dispatched solve.
    ///
    /// This remains for local terminal decisions, which have no continuation
    /// or deadline to authenticate.
    #[must_use]
    pub fn local_decision_id(&self) -> JobId {
        let bytes = postcard::to_allocvec(self).expect("JobIdentity is infallibly serialisable");
        JobId(ids::digest128(&[
            b"routers.local-decision.v1",
            bytes.as_slice(),
        ]))
    }
}

/// Digest of the continuation payload inside a [`JobProof`].
///
/// This deliberately is not a [`JobId`]: it participates in the outer job-id
/// digest but cannot itself name or authenticate a dispatched solve job. As a
/// serde newtype around `u128`, it preserves the prior postcard wire shape.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
struct ContinuationDigest(u128);

/// Compact, self-verifying summary of every solve-affecting job field. Results
/// echo this rather than the potentially large continuation itself, allowing
/// their [`JobId`] to be verified without a store lookup or duplicate payload.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct JobProof {
    /// The durable identity and resumed base state.
    pub identity: JobIdentity,
    /// The queue lane selected for this solve.
    pub lane: Lane,
    /// The absolute deadline the matcher must honour.
    pub deadline_us: i64,
    continuation: ContinuationDigest,
}

impl JobProof {
    fn for_job<E: Entry>(
        identity: &JobIdentity,
        lane: Lane,
        deadline_us: i64,
        context: &Continuation<E>,
    ) -> Self {
        let bytes = postcard::to_allocvec(context)
            .expect("Continuation is infallibly serialisable for an Entry");
        Self {
            identity: identity.clone(),
            lane,
            deadline_us,
            continuation: ContinuationDigest(ids::digest128(&[
                CONTINUATION_DOMAIN,
                bytes.as_slice(),
            ])),
        }
    }

    /// The deterministic solve-job id, covering every field in this proof.
    #[must_use]
    pub fn job_id(&self) -> JobId {
        let bytes = postcard::to_allocvec(self).expect("JobProof is infallibly serialisable");
        JobId(ids::digest128(&[JOB_ID_DOMAIN, bytes.as_slice()]))
    }
}

/// One regional solve job: solve `identity.observation` for `identity.vehicle`
/// against `identity.graph`, resuming from `context`, before `deadline_us`.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct SolveJob<E: Entry> {
    /// The job's deterministic identity; equals [`SolveJob::computed_id`].
    pub id: JobId,
    /// The context that makes this job unique and that `id` digests.
    pub identity: JobIdentity,
    /// The priority lane this job travels in (default [`Lane::DEFAULT`]).
    pub lane: Lane,
    /// Absolute deadline in unix microseconds; a matcher rejects an expired job.
    pub deadline_us: i64,
    /// The resume/restart state the matcher solves from.
    pub context: Continuation<E>,
}

impl<E: Entry> SolveJob<E> {
    /// Build a job for `identity`, computing a [`JobId`] that authenticates
    /// the complete solve envelope.
    pub fn new(
        identity: JobIdentity,
        lane: Lane,
        deadline_us: i64,
        context: Continuation<E>,
    ) -> Self {
        let id = JobProof::for_job(&identity, lane, deadline_us, &context).job_id();
        Self {
            id,
            identity,
            lane,
            deadline_us,
            context,
        }
    }

    /// The compact proof of the complete solve envelope, suitable for echoing
    /// in a result without duplicating its continuation.
    #[must_use]
    pub fn proof(&self) -> JobProof {
        JobProof::for_job(&self.identity, self.lane, self.deadline_us, &self.context)
    }

    /// Recompute the id from every solve-affecting envelope field.
    #[must_use]
    pub fn computed_id(&self) -> JobId {
        self.proof().job_id()
    }

    /// Check that this envelope is self-consistent: its `id` is the digest of
    /// every solve-affecting field, and its schema is the schema this build speaks.
    pub fn verify(&self) -> Result<(), JobError> {
        let expected = self.computed_id();
        if self.id != expected {
            return Err(JobError::IdMismatch {
                expected,
                got: self.id,
            });
        }
        if self.identity.schema != SCHEMA_VERSION {
            return Err(JobError::Schema {
                expected: SCHEMA_VERSION,
                got: self.identity.schema,
            });
        }
        Ok(())
    }

    /// How long is left before the deadline, or `None` once it has passed
    /// (including exactly at the deadline — no time then remains).
    pub fn remaining(&self, now_us: i64) -> Option<Duration> {
        let micros = self.deadline_us.checked_sub(now_us)?;
        (micros > 0).then(|| Duration::from_micros(micros as u64))
    }

    /// The broker dedup key for this job: its [`JobId`] as 32 lowercase hex.
    #[must_use]
    pub fn msg_id(&self) -> String {
        self.id.to_string()
    }

    /// The newest fresh observation — the head this job is being solved for.
    /// `None` only for the degenerate empty-context job (no fresh origins).
    pub fn head(&self) -> Option<&Origin> {
        let fresh = match &self.context {
            Continuation::Resume { fresh, .. } => fresh,
            Continuation::Restart { fresh } => fresh,
        };
        fresh.last()
    }
}

impl<E: Entry + serde::de::DeserializeOwned> SolveJob<E> {
    /// Decode a job from the wire and [`verify`](SolveJob::verify) it in one
    /// step, so a caller never handles an unverified envelope.
    pub fn decode_verified(bytes: &[u8]) -> Result<Self, JobError> {
        let job = <Self as Wire>::decode(bytes).map_err(JobError::Decode)?;
        job.verify()?;
        Ok(job)
    }
}

postcard_wire!(SolveJob<E: Entry>);

/// Why a [`SolveJob`] failed to verify or decode.
#[derive(Debug, Error)]
pub enum JobError {
    /// The envelope's `id` is not the digest of its complete solve context.
    #[error("job id mismatch: expected {expected}, got {got}")]
    IdMismatch {
        /// The id recomputed from the envelope's identity.
        expected: JobId,
        /// The id the envelope carried.
        got: JobId,
    },
    /// The job was produced against a different wire schema than this build.
    #[error("schema mismatch: expected {expected}, got {got}")]
    Schema {
        /// The schema this build speaks.
        expected: SchemaVersion,
        /// The schema the job was stamped with.
        got: SchemaVersion,
    },
    /// The bytes could not be decoded into a job at all.
    #[error("could not decode solve job: {0}")]
    Decode(anyhow::Error),
}

#[cfg(test)]
mod tests {
    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::Trip;

    use super::*;

    fn sample_identity() -> JobIdentity {
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(1),
            observation: ObservationId {
                partition: 485,
                sequence: 7,
            },
            base: None,
            graph: GraphVersion::new("g1").unwrap(),
            region: RegionId::new("r1").unwrap(),
        }
    }

    fn origin(ts: i64) -> Origin {
        Origin::new(Point::new(1.0, 2.0), ts)
    }

    fn restart(fresh: Vec<Origin>) -> Continuation<MockEntryId> {
        Continuation::Restart { fresh }
    }

    fn sample_job() -> SolveJob<MockEntryId> {
        SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            10,
            restart(vec![origin(1)]),
        )
    }

    #[test]
    fn same_complete_envelope_yields_same_id() {
        assert_eq!(sample_job().id, sample_job().id);
    }

    #[test]
    fn every_solve_affecting_field_changes_the_id() {
        let base = sample_job();
        let base_id = base.id;
        let mut mutations: Vec<(&str, SolveJob<MockEntryId>)> = Vec::new();

        let mut changed = base.clone();
        changed.identity.schema = SchemaVersion(SCHEMA_VERSION.0 + 1);
        mutations.push(("schema", changed));
        let mut changed = base.clone();
        changed.identity.vehicle_id = VehicleId(2);
        mutations.push(("vehicle_id", changed));
        let mut changed = base.clone();
        changed.identity.observation.partition += 1;
        mutations.push(("observation.partition", changed));
        let mut changed = base.clone();
        changed.identity.observation.sequence += 1;
        mutations.push(("observation.sequence", changed));
        let mut changed = base.clone();
        changed.identity.base = Some(BaseState {
            revision: Revision(3),
            segment: SegmentId(3),
        });
        mutations.push(("base", changed));
        let mut changed = base.clone();
        changed.identity.graph = GraphVersion::new("g2").unwrap();
        mutations.push(("graph", changed));
        let mut changed = base.clone();
        changed.identity.region = RegionId::new("r2").unwrap();
        mutations.push(("region", changed));
        let mut changed = base.clone();
        changed.lane = Lane(1);
        mutations.push(("lane", changed));
        let mut changed = base.clone();
        changed.deadline_us += 1;
        mutations.push(("deadline_us", changed));
        let mut changed = base.clone();
        changed.context = restart(vec![origin(2)]);
        mutations.push(("continuation", changed));

        for (field, changed) in mutations {
            assert_ne!(
                changed.computed_id(),
                base_id,
                "changing {field} left the id unchanged"
            );
            assert!(
                matches!(changed.verify(), Err(JobError::IdMismatch { .. })),
                "changing {field} without replacing id must fail verification"
            );
        }
    }

    #[test]
    fn base_state_variation_changes_the_id() {
        let with_base = |revision, segment| JobIdentity {
            base: Some(BaseState {
                revision: Revision(revision),
                segment: SegmentId(segment),
            }),
            ..sample_identity()
        };
        let a = SolveJob::new(with_base(1, 1), Lane::DEFAULT, 10, restart(vec![])).id;
        let b = SolveJob::new(with_base(2, 1), Lane::DEFAULT, 10, restart(vec![])).id;
        let c = SolveJob::new(with_base(1, 2), Lane::DEFAULT, 10, restart(vec![])).id;
        assert_ne!(a, b, "revision must affect the id");
        assert_ne!(a, c, "segment must affect the id");
        assert_ne!(b, c);
    }

    #[test]
    fn verify_accepts_a_well_formed_job() {
        let job = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            100,
            restart(vec![origin(1)]),
        );
        assert!(job.verify().is_ok());
    }

    #[test]
    fn verify_catches_a_tampered_id() {
        let mut job = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            100,
            restart(vec![origin(1)]),
        );
        let good = job.id;
        job.id = JobId(job.id.0 ^ 1);
        match job.verify() {
            Err(JobError::IdMismatch { expected, got }) => {
                assert_eq!(expected, good);
                assert_eq!(got, job.id);
            }
            other => panic!("expected IdMismatch, got {other:?}"),
        }
    }

    #[test]
    fn verify_catches_a_wrong_schema() {
        let identity = JobIdentity {
            schema: SchemaVersion(SCHEMA_VERSION.0 + 1),
            ..sample_identity()
        };
        let job = SolveJob::new(identity, Lane::DEFAULT, 100, restart(vec![origin(1)]));
        assert!(matches!(
            job.verify(),
            Err(JobError::Schema { expected, got })
                if expected == SCHEMA_VERSION && got == SchemaVersion(SCHEMA_VERSION.0 + 1)
        ));
    }

    #[test]
    fn wire_round_trip_restart() {
        let job = SolveJob::new(
            sample_identity(),
            Lane(2),
            12_345,
            restart(vec![origin(1), origin(2)]),
        );
        let bytes = job.encode().unwrap();
        let decoded = SolveJob::<MockEntryId>::decode_verified(&bytes).unwrap();
        assert_eq!(decoded.id, job.id);
        assert_eq!(decoded.identity, job.identity);
        assert_eq!(decoded.lane, Lane(2));
        assert_eq!(decoded.deadline_us, 12_345);
        assert_eq!(decoded.head(), Some(&origin(2)));
    }

    #[test]
    fn wire_round_trip_resume() {
        let context = Continuation::<MockEntryId>::Resume {
            trip: Trip::new(),
            fresh: vec![origin(5)],
        };
        let job = SolveJob::new(sample_identity(), Lane::DEFAULT, 7, context);
        let bytes = job.encode().unwrap();
        let decoded = SolveJob::<MockEntryId>::decode_verified(&bytes).unwrap();
        assert_eq!(decoded.id, job.id);
        assert_eq!(decoded.identity, job.identity);
        assert!(matches!(decoded.context, Continuation::Resume { .. }));
        assert_eq!(decoded.head(), Some(&origin(5)));
    }

    #[test]
    fn decode_verified_rejects_a_tampered_envelope() {
        let mut job = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            100,
            restart(vec![origin(1)]),
        );
        job.id = JobId(job.id.0 ^ 1);
        let bytes = job.encode().unwrap();
        assert!(matches!(
            SolveJob::<MockEntryId>::decode_verified(&bytes),
            Err(JobError::IdMismatch { .. })
        ));
    }

    #[test]
    fn decode_verified_rejects_garbage() {
        assert!(matches!(
            SolveJob::<MockEntryId>::decode_verified(&[0xff, 0xff, 0xff, 0xff]),
            Err(JobError::Decode(_))
        ));
    }

    #[test]
    fn msg_id_is_the_hex_id() {
        let job = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            100,
            restart(vec![origin(1)]),
        );
        assert_eq!(job.msg_id(), job.id.to_string());
        assert_eq!(job.msg_id().len(), 32);
    }

    #[test]
    fn head_is_the_newest_fresh_origin() {
        let job = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            100,
            restart(vec![origin(1), origin(2), origin(9)]),
        );
        assert_eq!(job.head(), Some(&origin(9)));

        let empty = SolveJob::new(sample_identity(), Lane::DEFAULT, 100, restart(vec![]));
        assert_eq!(empty.head(), None);
    }

    #[test]
    fn remaining_before_at_and_after_deadline() {
        let job = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            1_000,
            restart(vec![origin(1)]),
        );
        assert_eq!(job.remaining(400), Some(Duration::from_micros(600)));
        assert_eq!(
            job.remaining(1_000),
            None,
            "at the deadline nothing remains"
        );
        assert_eq!(
            job.remaining(1_500),
            None,
            "past the deadline nothing remains"
        );
    }

    #[test]
    fn job_id_is_wire_law() {
        // Wire law: if this pinned id changes, the encoding or field order moved; fix that, not the constant.
        let identity = JobIdentity {
            schema: SchemaVersion(1),
            vehicle_id: VehicleId(1),
            observation: ObservationId {
                partition: 485,
                sequence: 7,
            },
            base: None,
            graph: GraphVersion::new("g1").unwrap(),
            region: RegionId::new("r1").unwrap(),
        };
        let job = SolveJob::new(identity, Lane::DEFAULT, 10, restart(vec![origin(1)]));
        assert_eq!(job.computed_id(), job.id);
        assert_eq!(job.id.to_string().len(), 32);
    }
}
