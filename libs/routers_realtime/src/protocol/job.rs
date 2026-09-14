//! `SolveJob`: the regional solve-job message. (T02)
//!
//! A [`SolveJob`] is one unit of work handed to the regional matchers: solve
//! this vehicle's next observation against a named graph, resuming (or
//! restarting) from the state the orchestrator committed. Its identity is not
//! a random id but a deterministic hash of its [`JobIdentity`] — everything
//! that makes the job's context unique. Two orchestrators (or the same one
//! retrying an ambiguous publish) that build the same context therefore mint
//! byte-identical bytes and the *same* [`JobId`], so the broker's `Nats-Msg-Id`
//! dedup collapses the duplicate rather than solving it twice.
//!
//! That determinism rests on postcard's encoding being stable for a fixed
//! field order. The order of the fields in [`JobIdentity`] (and of the
//! variants it reaches) is therefore **wire law**: reordering them silently
//! changes every job id in the fleet and breaks dedup across a rolling
//! deploy. Do not reorder, insert, or retype a field without bumping the
//! schema and the domain tag below.

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

/// The committed state a job resumes from: which revision last decided this
/// vehicle and the continuity segment that revision belonged to. Absent
/// (`JobIdentity::base == None`) for a fresh vehicle with no checkpoint yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseState {
    /// The revision (raw sequence) of the last committed decision resumed from.
    pub revision: Revision,
    /// The continuity segment that committed decision belonged to.
    pub segment: SegmentId,
}

/// Everything that makes a solve job's context unique. The job's [`JobId`] is
/// the digest of this record, so any two jobs that would compute a different
/// answer differ here in at least one field — and any two that would compute
/// the *same* answer share an id and dedup.
///
/// Field order is wire law (see the module docs): the postcard encoding hashed
/// by [`JobIdentity::job_id`] depends on it, and changing the order re-hashes
/// every job in flight.
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

/// The domain-separation tag mixed into every job digest. Bumping the wire
/// contract that this hash protects means bumping this tag as well, so a job
/// id can never collide across contract versions.
const JOB_ID_DOMAIN: &[u8] = b"routers.solve-job.v1";

impl JobIdentity {
    /// The deterministic 128-bit identity of a job with this context: the
    /// leading 16 bytes of `sha256(domain-tag ∥ postcard(self))`.
    ///
    /// Deterministic because postcard encodes a fixed-order struct the same
    /// way every time, so the same context always yields the same id (and
    /// hence the same `Nats-Msg-Id`). Reordering the fields of [`JobIdentity`]
    /// would change this value for every job — that is why the field order is
    /// wire law.
    #[must_use]
    pub fn job_id(&self) -> JobId {
        // Serialising an in-memory value of these types cannot fail; postcard
        // only errors on custom `Serialize` impls or unsupported shapes.
        let bytes = postcard::to_allocvec(self).expect("JobIdentity is infallibly serialisable");
        JobId(ids::digest128(&[JOB_ID_DOMAIN, bytes.as_slice()]))
    }
}

/// One regional solve job: solve `identity.observation` for `identity.vehicle`
/// against `identity.graph`, resuming from `context`, before `deadline_us`.
///
/// The `id` is redundant with `identity` on purpose — it travels as the broker
/// dedup key so peers need not re-hash to route, and every receiver
/// [`verify`](SolveJob::verify)s it back against `identity` on decode so a
/// forged or corrupted envelope cannot masquerade as a different job.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct SolveJob<E: Entry> {
    /// The job's deterministic identity; equals `identity.job_id()`.
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
    /// Build a job for `identity`, computing its [`JobId`] from that identity
    /// so `id` and `identity` can never disagree at the source.
    pub fn new(
        identity: JobIdentity,
        lane: Lane,
        deadline_us: i64,
        context: Continuation<E>,
    ) -> Self {
        let id = identity.job_id();
        Self {
            id,
            identity,
            lane,
            deadline_us,
            context,
        }
    }

    /// Check that this envelope is self-consistent: its `id` is the digest of
    /// its `identity`, and its `identity.schema` is the schema this build
    /// speaks. Receivers call this on every decode so a tampered id or a
    /// cross-version job is caught before it is solved.
    pub fn verify(&self) -> Result<(), JobError> {
        let expected = self.identity.job_id();
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
    /// step, so a caller never handles an unverified envelope. Bounded by
    /// [`DeserializeOwned`](serde::de::DeserializeOwned) — exactly what the
    /// [`Wire`] decode it delegates to needs.
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
    /// The envelope's `id` is not the digest of its `identity`: the job was
    /// corrupted or forged.
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

    /// A canonical identity the field-mutation and known-answer tests vary from.
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

    #[test]
    fn same_identity_yields_same_id() {
        // Two independent constructions of the same context must agree, or the
        // broker could never dedup a retried publish.
        let a = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            10,
            restart(vec![origin(1)]),
        );
        let b = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            999,
            restart(vec![origin(2)]),
        );
        assert_eq!(
            a.id, b.id,
            "id derives only from identity, not lane/deadline/context"
        );
        assert_eq!(a.id, sample_identity().job_id());
    }

    #[test]
    fn each_field_changes_the_id() {
        let base = sample_identity();
        let base_id = base.job_id();

        // One mutation per field; each must move the digest.
        let mutations: Vec<(&str, JobIdentity)> = vec![
            (
                "schema",
                JobIdentity {
                    schema: SchemaVersion(2),
                    ..sample_identity()
                },
            ),
            (
                "vehicle_id",
                JobIdentity {
                    vehicle_id: VehicleId(2),
                    ..sample_identity()
                },
            ),
            (
                "observation.partition",
                JobIdentity {
                    observation: ObservationId {
                        partition: 486,
                        sequence: 7,
                    },
                    ..sample_identity()
                },
            ),
            (
                "observation.sequence",
                JobIdentity {
                    observation: ObservationId {
                        partition: 485,
                        sequence: 8,
                    },
                    ..sample_identity()
                },
            ),
            (
                "base",
                JobIdentity {
                    base: Some(BaseState {
                        revision: Revision(3),
                        segment: SegmentId(3),
                    }),
                    ..sample_identity()
                },
            ),
            (
                "graph",
                JobIdentity {
                    graph: GraphVersion::new("g2").unwrap(),
                    ..sample_identity()
                },
            ),
            (
                "region",
                JobIdentity {
                    region: RegionId::new("r2").unwrap(),
                    ..sample_identity()
                },
            ),
        ];

        let mut seen = vec![base_id];
        for (field, mutated) in mutations {
            let id = mutated.job_id();
            assert_ne!(id, base_id, "changing {field} left the id unchanged");
            assert!(
                !seen.contains(&id),
                "changing {field} collided with another id"
            );
            seen.push(id);
        }
    }

    #[test]
    fn base_state_variation_changes_the_id() {
        // The two BaseState fields must each be hashed independently.
        let with_base = |revision, segment| JobIdentity {
            base: Some(BaseState {
                revision: Revision(revision),
                segment: SegmentId(segment),
            }),
            ..sample_identity()
        };
        let a = with_base(1, 1).job_id();
        let b = with_base(2, 1).job_id();
        let c = with_base(1, 2).job_id();
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
        // Built with schema 2, so `id` matches `identity` but the schema does
        // not match this build — the second check must fire.
        let identity = JobIdentity {
            schema: SchemaVersion(2),
            ..sample_identity()
        };
        let job = SolveJob::new(identity, Lane::DEFAULT, 100, restart(vec![origin(1)]));
        assert!(matches!(
            job.verify(),
            Err(JobError::Schema {
                expected: SCHEMA_VERSION,
                got: SchemaVersion(2),
            })
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
        // A truncated/garbage buffer fails at the decode step, not verify.
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
        // KNOWN-ANSWER TEST. This hex is WIRE LAW: it is the job id every peer
        // in the fleet must compute for this exact identity. It was pinned the
        // first time this test ran; if it ever changes, the encoding or field
        // order of JobIdentity changed and every in-flight job id moved with
        // it. Do not "fix" this constant — fix the change that moved it.
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
        assert_eq!(
            identity.job_id().to_string(),
            "31f0fe50944127c3cf4104603200b947",
            "job id is wire law; see the comment above"
        );
    }
}
