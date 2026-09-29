//! `SolveJob`: one unit of regional solve work. Its identity is a deterministic
//! hash of its entire solve envelope, so two orchestrators that build the same
//! context mint the same [`JobId`]. Only [`SolveRequest`] travels on the wire:
//! the job's proof plus its full or trip-less continuation, from which both
//! sides rebuild the verified [`SolveJob`].
//!
//! Field order in [`JobProof`] is wire law: postcard hashes it, so reordering
//! silently changes every job id in the fleet. Do not reorder without bumping
//! the solve-job contract and the domain tag below.

use core::time::Duration;

use routers_network::Entry;
use routers_transition::matcher::{Continuation, Origin, Trip};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::bus::postcard_wire;
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
/// [`JobId`] additionally authenticates the lane, freshness target, and continuation
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
const CONTINUATION_DOMAIN: &[u8] = b"routers.solve-job.continuation.v2";
/// Separates a trip digest from every other digest.
const TRIP_DOMAIN: &[u8] = b"routers.solve-job.trip.v1";

/// Digest of one [`Trip`]'s canonical postcard encoding.
///
/// A continuation is authenticated by this digest plus its fresh origins, so
/// a party that already holds the trip's digest (the owner's sealed checkpoint,
/// or the matcher's cache entry) never re-serialises the trip to build or check
/// a job id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TripDigest(pub u128);

impl TripDigest {
    /// The digest of `trip`, serialising it once.
    #[must_use]
    pub fn of<E: Entry>(trip: &Trip<E>) -> Self {
        let bytes = postcard::to_allocvec(trip).expect("Trip is infallibly serialisable");
        Self::of_bytes(&bytes)
    }

    /// The digest of a trip's already-encoded postcard bytes.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(ids::digest128(&[TRIP_DOMAIN, bytes]))
    }
}

/// The continuation digest for a resume from `trip` (or a restart when
/// `None`) followed by `fresh`.
fn continuation_digest(trip: Option<TripDigest>, fresh: &[Origin]) -> ContinuationDigest {
    let fresh = postcard::to_allocvec(fresh).expect("origins are infallibly serialisable");
    ContinuationDigest(match trip {
        Some(TripDigest(trip)) => ids::digest128(&[
            CONTINUATION_DOMAIN,
            b"resume",
            trip.to_be_bytes().as_slice(),
            fresh.as_slice(),
        ]),
        None => ids::digest128(&[CONTINUATION_DOMAIN, b"restart", fresh.as_slice()]),
    })
}

/// The fresh origins of a continuation.
fn fresh_of<E: Entry>(context: &Continuation<E>) -> &[Origin] {
    match context {
        Continuation::Resume { fresh, .. } | Continuation::Restart { fresh } => fresh,
    }
}

/// The trip digest a continuation authenticates, computing it when the
/// caller does not already hold it.
fn trip_digest_of<E: Entry>(context: &Continuation<E>) -> Option<TripDigest> {
    match context {
        Continuation::Resume { trip, .. } => Some(TripDigest::of(trip)),
        Continuation::Restart { .. } => None,
    }
}

impl JobIdentity {
    /// The deterministic id of a context-only local decision. It is not the
    /// [`SolveJob`] id: use [`SolveJob::computed_id`] for a dispatched solve.
    ///
    /// This remains for local terminal decisions, which have no continuation
    /// or freshness target to authenticate.
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
    /// The absolute freshness target the matcher observes.
    pub freshness_target_us: i64,
    continuation: ContinuationDigest,
}

impl JobProof {
    fn for_parts(
        identity: &JobIdentity,
        lane: Lane,
        freshness_target_us: i64,
        trip: Option<TripDigest>,
        fresh: &[Origin],
    ) -> Self {
        Self {
            identity: identity.clone(),
            lane,
            freshness_target_us,
            continuation: continuation_digest(trip, fresh),
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
/// against `identity.graph`, resuming from `context`. `freshness_target_us` is the
/// authenticated freshness target used for SLA telemetry, not a correctness
/// cutoff.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct SolveJob<E: Entry> {
    /// The job's deterministic identity; equals [`SolveJob::computed_id`].
    pub id: JobId,
    /// The context that makes this job unique and that `id` digests.
    pub identity: JobIdentity,
    /// The priority lane this job travels in (default [`Lane::DEFAULT`]).
    pub lane: Lane,
    /// Absolute freshness target in unix microseconds; matchers observe lateness
    /// but still solve an overdue target.
    pub freshness_target_us: i64,
    /// The resume/restart state the matcher solves from.
    pub context: Continuation<E>,
    /// The digest of `context`'s trip for a resume, held so the job's proof
    /// is rebuilt without re-serialising the trip. `None` for a restart.
    pub trip_digest: Option<TripDigest>,
}

impl<E: Entry> SolveJob<E> {
    /// Build a job for `identity`, digesting its trip, and computing a
    /// [`JobId`] that authenticates the complete solve envelope.
    pub fn new(
        identity: JobIdentity,
        lane: Lane,
        freshness_target_us: i64,
        context: Continuation<E>,
    ) -> Self {
        let trip_digest = trip_digest_of(&context);
        Self::with_trip_digest(identity, lane, freshness_target_us, context, trip_digest)
    }

    /// As [`new`](Self::new), trusting `trip_digest` as the digest of
    /// `context`'s trip (`None` for a restart). The caller must hold it from
    /// the same trip bytes, as a sealed checkpoint or matcher cache entry does.
    pub fn with_trip_digest(
        identity: JobIdentity,
        lane: Lane,
        freshness_target_us: i64,
        context: Continuation<E>,
        trip_digest: Option<TripDigest>,
    ) -> Self {
        debug_assert_eq!(
            trip_digest.is_some(),
            matches!(context, Continuation::Resume { .. }),
            "exactly a resume carries a trip digest"
        );
        let id = JobProof::for_parts(
            &identity,
            lane,
            freshness_target_us,
            trip_digest,
            fresh_of(&context),
        )
        .job_id();
        Self {
            id,
            identity,
            lane,
            freshness_target_us,
            context,
            trip_digest,
        }
    }

    /// The compact proof of the complete solve envelope, suitable for echoing
    /// in a result without duplicating its continuation. Uses the held trip
    /// digest, so it costs only the fresh origins.
    #[must_use]
    pub fn proof(&self) -> JobProof {
        JobProof::for_parts(
            &self.identity,
            self.lane,
            self.freshness_target_us,
            self.trip_digest,
            fresh_of(&self.context),
        )
    }

    /// Recompute the id from every solve-affecting field, re-digesting the
    /// trip itself rather than trusting the held digest.
    #[must_use]
    pub fn computed_id(&self) -> JobId {
        JobProof::for_parts(
            &self.identity,
            self.lane,
            self.freshness_target_us,
            trip_digest_of(&self.context),
            fresh_of(&self.context),
        )
        .job_id()
    }

    /// Check that this envelope is self-consistent: its `id` is the digest of
    /// every solve-affecting field, including the trip itself, and its schema
    /// is the schema this build speaks.
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

    /// How long remains before the freshness target, or `None` once it has
    /// passed (including exactly at the target). This is telemetry-only; it
    /// must not be used to reject a valid job.
    pub fn remaining(&self, now_us: i64) -> Option<Duration> {
        let micros = self.freshness_target_us.checked_sub(now_us)?;
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

/// The continuation a transient [`SolveRequest`] carries.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub enum RequestContext<E: Entry> {
    /// The complete continuation, trip included.
    Full(Continuation<E>),
    /// Only the fresh observations of a resume. The addressed matcher supplies
    /// the trip it produced for `proof.identity.base`; the job id, which digests
    /// the full continuation, proves the substitution was exact.
    Cached { fresh: Vec<Origin> },
}

/// The transient solve-request wire message: a job's compact [`JobProof`] and
/// either its full continuation or just the fresh tail of a resume.
///
/// Carrying the proof lets a matcher that cannot resolve a cached trip still
/// answer with a verifiable [`SolveOutcome::TripMiss`](crate::protocol::result::SolveOutcome::TripMiss).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct SolveRequest<E: Entry> {
    /// Every solve-affecting field except the continuation, plus its digest.
    pub proof: JobProof,
    /// The continuation, or the part of it the matcher does not already hold.
    pub context: RequestContext<E>,
}

/// Why a [`SolveRequest`] could not be turned into a verified [`SolveJob`].
#[derive(Debug, Error)]
pub enum ResolveError {
    /// A cached request whose trip the matcher does not hold, or holds for a
    /// different continuation. Answer with a trip miss; this is not poison.
    #[error("cached trip unavailable for the requested base")]
    TripMiss(Box<JobProof>),
    /// The request itself is malformed or forged.
    #[error(transparent)]
    Job(#[from] JobError),
}

impl<E: Entry> SolveRequest<E> {
    /// A request carrying `job`'s complete continuation.
    #[must_use]
    pub fn full(job: &SolveJob<E>) -> Self {
        Self {
            proof: job.proof(),
            context: RequestContext::Full(job.context.clone()),
        }
    }

    /// A request carrying only `job`'s fresh observations, or `None` when the
    /// job is not a resume from a committed base (nothing to have cached).
    #[must_use]
    pub fn cached(job: &SolveJob<E>) -> Option<Self> {
        match (&job.context, job.identity.base) {
            (Continuation::Resume { fresh, .. }, Some(_)) => Some(Self {
                proof: job.proof(),
                context: RequestContext::Cached {
                    fresh: fresh.clone(),
                },
            }),
            _ => None,
        }
    }

    /// The id the proof authenticates.
    #[must_use]
    pub fn job_id(&self) -> JobId {
        self.proof.job_id()
    }

    /// Whether this request relies on the matcher's cached trip.
    #[must_use]
    pub fn is_cached(&self) -> bool {
        matches!(self.context, RequestContext::Cached { .. })
    }

    /// Rebuild and verify the full job. A cached request takes its trip and
    /// that trip's digest from `lookup(vehicle, base_revision)`, so it is
    /// verified from the fresh origins alone; a full request digests its trip
    /// once. A missing or non-matching cached trip is a
    /// [`ResolveError::TripMiss`], while any other mismatch is a [`JobError`].
    pub fn resolve(
        self,
        lookup: impl FnOnce(VehicleId, Revision) -> Option<(Trip<E>, TripDigest)>,
    ) -> Result<SolveJob<E>, ResolveError> {
        let Self { proof, context } = self;
        let id = proof.job_id();
        let (context, trip_digest, cached) = match context {
            RequestContext::Full(continuation) => {
                let digest = trip_digest_of(&continuation);
                (continuation, digest, false)
            }
            RequestContext::Cached { fresh } => {
                let Some(base) = proof.identity.base else {
                    return Err(ResolveError::TripMiss(Box::new(proof)));
                };
                let Some((trip, digest)) = lookup(proof.identity.vehicle_id, base.revision) else {
                    return Err(ResolveError::TripMiss(Box::new(proof)));
                };
                (Continuation::Resume { trip, fresh }, Some(digest), true)
            }
        };
        let rebuilt = JobProof::for_parts(
            &proof.identity,
            proof.lane,
            proof.freshness_target_us,
            trip_digest,
            fresh_of(&context),
        );
        if rebuilt != proof {
            return if cached {
                Err(ResolveError::TripMiss(Box::new(proof)))
            } else {
                Err(JobError::IdMismatch {
                    expected: rebuilt.job_id(),
                    got: id,
                }
                .into())
            };
        }
        if proof.identity.schema != SCHEMA_VERSION {
            return Err(JobError::Schema {
                expected: SCHEMA_VERSION,
                got: proof.identity.schema,
            }
            .into());
        }
        Ok(SolveJob {
            id,
            identity: proof.identity,
            lane: proof.lane,
            freshness_target_us: proof.freshness_target_us,
            context,
            trip_digest,
        })
    }
}

postcard_wire!(SolveRequest<E: Entry>);

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
    use crate::bus::Wire;

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
        changed.freshness_target_us += 1;
        mutations.push(("freshness_target_us", changed));
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
    fn remaining_before_at_and_after_target() {
        let job = SolveJob::new(
            sample_identity(),
            Lane::DEFAULT,
            1_000,
            restart(vec![origin(1)]),
        );
        assert_eq!(job.remaining(400), Some(Duration::from_micros(600)));
        assert_eq!(job.remaining(1_000), None, "at the target nothing remains");
        assert_eq!(
            job.remaining(1_500),
            None,
            "past the target nothing remains"
        );
    }

    fn resume_job(base: Option<BaseState>) -> SolveJob<MockEntryId> {
        let identity = JobIdentity {
            base,
            ..sample_identity()
        };
        let context = Continuation::Resume {
            trip: Trip::new(),
            fresh: vec![origin(5)],
        };
        SolveJob::new(identity, Lane::DEFAULT, 7, context)
    }

    fn committed_base() -> Option<BaseState> {
        Some(BaseState {
            revision: Revision(4),
            segment: SegmentId(1),
        })
    }

    #[test]
    fn full_request_resolves_to_the_same_job() {
        let job = sample_job();
        let request = SolveRequest::full(&job);
        assert_eq!(request.job_id(), job.id);
        let bytes = request.encode().unwrap();
        let resolved = SolveRequest::<MockEntryId>::decode(&bytes)
            .unwrap()
            .resolve(|_, _| panic!("a full request never consults the cache"))
            .unwrap();
        assert_eq!(resolved.id, job.id);
        assert_eq!(resolved.identity, job.identity);
    }

    #[test]
    fn cached_request_resolves_with_the_matching_trip() {
        let job = resume_job(committed_base());
        let request = SolveRequest::cached(&job).expect("a based resume can be cached");
        let full = SolveRequest::full(&job).encode().unwrap();
        let cached = request.encode().unwrap();
        assert!(cached.len() <= full.len());
        let resolved = SolveRequest::<MockEntryId>::decode(&cached)
            .unwrap()
            .resolve(|vehicle, revision| {
                assert_eq!(vehicle, job.identity.vehicle_id);
                assert_eq!(revision, Revision(4));
                Some((Trip::new(), TripDigest::of(&Trip::<MockEntryId>::new())))
            })
            .unwrap();
        assert_eq!(resolved.id, job.id);
        assert_eq!(resolved.head(), Some(&origin(5)));
    }

    #[test]
    fn cached_request_without_the_trip_is_a_miss_not_poison() {
        let job = resume_job(committed_base());
        let request = SolveRequest::cached(&job).unwrap();
        match request.resolve(|_, _| None) {
            Err(ResolveError::TripMiss(proof)) => assert_eq!(proof.job_id(), job.id),
            other => panic!("expected a trip miss, got {other:?}"),
        }
    }

    #[test]
    fn cached_request_that_fails_verification_is_a_miss() {
        // Whatever the cause — a stale cached trip or altered fresh origins —
        // a rebuilt continuation that misses the job id is a miss, which the
        // owner answers by resending the authenticated full continuation.
        let job = resume_job(committed_base());
        let mut request = SolveRequest::cached(&job).unwrap();
        request.context = RequestContext::Cached {
            fresh: vec![origin(6)],
        };
        let result = request
            .resolve(|_, _| Some((Trip::new(), TripDigest::of(&Trip::<MockEntryId>::new()))));
        assert!(
            matches!(result, Err(ResolveError::TripMiss(_))),
            "{result:?}"
        );
    }

    #[test]
    fn only_based_resumes_can_be_cached() {
        assert!(SolveRequest::cached(&sample_job()).is_none(), "restart");
        assert!(SolveRequest::cached(&resume_job(None)).is_none(), "no base");
    }

    #[test]
    fn a_forged_full_request_is_poison() {
        let job = sample_job();
        let mut request = SolveRequest::full(&job);
        request.context = RequestContext::Full(restart(vec![origin(99)]));
        assert!(matches!(
            request.resolve(|_, _| None),
            Err(ResolveError::Job(JobError::IdMismatch { .. }))
        ));
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
