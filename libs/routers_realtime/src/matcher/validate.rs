//! Matcher job validator: classify one pulled [`SolveJob`] into solve, refuse,
//! or poison, so a matcher never solves work it cannot answer correctly. (T25)
//!
//! A matcher pulls jobs off a work queue shared by every replica of its region,
//! and some of what it pulls it must not solve: a job cut against a graph this
//! replica does not hold, addressed to another region, over a cell outside the
//! loaded coverage, or already past its deadline. Solving such a job would
//! either produce an answer against the wrong network or burn the CPU the queue
//! is short of on a result nobody can use. The spec's rule (§4 "Job validator",
//! §8) is that an invalid job yields a *typed unsuccessful result*, never
//! silence — the orchestrator that dispatched it learns why instead of waiting
//! the deadline out. This module is the pure decision that sorts a job into
//! exactly one of:
//!
//! * [`Checked::Solve`] — hand it to the engine.
//! * [`Checked::Refuse`] — do not solve it; publish the carried [`SolveOutcome`]
//!   as the job's result so the owner sees a definite, typed answer.
//! * [`Checked::Poison`] — the bytes could not even be turned into a job we
//!   could address a result to (undecodable, a forged id, the wrong schema, or
//!   too large to trust before decoding); acknowledge and drop it, because
//!   there is no identity to publish a result against.
//!
//! The wire-level checks ([`check_bytes`], which owns the decode) are split from
//! the semantic checks ([`check`], which reasons about an already-decoded job)
//! so that [`check`] stays a pure, network-free function: it takes the region
//! and its served cell set directly rather than a live `Loaded` graph, and a
//! test drives it with a hand-built [`Region`] and [`HashSet`] of cells.

use core::time::Duration;
use std::collections::HashSet;

use routers_network::Entry;
use routers_shard::Geohash;
use routers_transition::matcher::{Continuation, Origin};
use thiserror::Error;

use crate::event::{self, VehicleId};
use crate::protocol::ids::SchemaVersion;
use crate::protocol::job::{JobError, SolveJob};
use crate::protocol::result::SolveOutcome;
use crate::region::Region;

// `VehicleId` is named only in doc links; keep the import honest.
#[allow(unused_imports)]
use self::VehicleId as _KeepVehicleIdDocLink;

/// How strict the validator is: the two bounds it enforces that are policy, not
/// correctness.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidateConfig {
    /// The largest wire buffer the matcher will decode. Enforced *before*
    /// decoding, so a hostile or corrupt length prefix can never make the
    /// process allocate an unbounded job (see [`check_bytes`]).
    pub max_decoded_bytes: usize,
    /// The least time that must remain before the deadline for a solve to be
    /// worth starting. A solve that cannot finish before its result is useless
    /// only spends the CPU the queue is already short of, so a job with less
    /// than this left is refused as expired rather than begun.
    pub min_remaining: Duration,
}

impl ValidateConfig {
    /// The default decode bound: 4 MiB, matching the matcher's job-size cap in
    /// the design brief (§9).
    pub const DEFAULT_MAX_DECODED_BYTES: usize = 4 << 20;
    /// The default deadline floor: 250 ms. Below this a solve is not worth
    /// starting.
    pub const DEFAULT_MIN_REMAINING: Duration = Duration::from_millis(250);
}

impl Default for ValidateConfig {
    fn default() -> Self {
        Self {
            max_decoded_bytes: Self::DEFAULT_MAX_DECODED_BYTES,
            min_remaining: Self::DEFAULT_MIN_REMAINING,
        }
    }
}

/// Why a job's bytes could not be turned into a result-bearing job at all.
///
/// A poison job is acknowledged and dropped rather than answered: unlike a
/// [`Checked::Refuse`], there is no trustworthy [`JobId`](crate::protocol::ids::JobId)
/// / identity to address a [`SolveResult`](crate::protocol::result::SolveResult)
/// to, so publishing one would be guessing. Every variant here is a fault of
/// the bytes on the wire, not of the vehicle being solved.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PoisonReason {
    /// The buffer did not decode into a [`SolveJob`] at all (truncated or
    /// corrupt). The text is the underlying decode error, for logs only.
    #[error("could not decode solve job: {0}")]
    Decode(String),
    /// The envelope decoded, but its `id` was not the digest of its identity:
    /// the job was corrupted or forged in flight and cannot be trusted.
    #[error("job id does not match its identity")]
    IdMismatch,
    /// The job was produced against a wire schema this build does not speak, so
    /// its fields cannot be interpreted safely.
    #[error("job was produced against schema {got}, which this build does not speak")]
    Schema {
        /// The schema the job carried.
        got: SchemaVersion,
    },
    /// The raw buffer exceeded [`ValidateConfig::max_decoded_bytes`] and was
    /// rejected *before* decoding. Because it was never decoded there is no
    /// identity to answer, so oversize-before-decode is poison; an over-limit
    /// job we *had* decoded would instead be a
    /// [`Refuse`](Checked::Refuse)`(`[`SolveOutcome::Oversized`]`)` that the
    /// owner can see.
    #[error("job envelope is {bytes} bytes, over the {limit}-byte decode bound")]
    Oversized {
        /// The size of the buffer that was refused.
        bytes: usize,
        /// The bound it broke.
        limit: usize,
    },
}

impl From<JobError> for PoisonReason {
    fn from(err: JobError) -> Self {
        match err {
            JobError::IdMismatch { .. } => PoisonReason::IdMismatch,
            JobError::Schema { got, .. } => PoisonReason::Schema { got },
            JobError::Decode(source) => PoisonReason::Decode(source.to_string()),
        }
    }
}

/// The verdict on one pulled job. Borrows the job for the [`Solve`](Self::Solve)
/// case so the caller keeps ownership and hands the very same value to the
/// engine.
///
/// [`check`] only ever yields [`Solve`](Self::Solve) or [`Refuse`](Self::Refuse);
/// [`Poison`](Self::Poison) is produced by [`check_bytes`] (as its `Err`) and is
/// carried here so a caller can fold both stages into one vocabulary.
#[derive(Debug)]
pub enum Checked<'j, E: Entry> {
    /// The job passed every check: solve it.
    Solve(&'j SolveJob<E>),
    /// The job must not be solved; publish this typed outcome as its result.
    Refuse(SolveOutcome<E>),
    /// The bytes could not be turned into an answerable job; ack and drop.
    Poison(PoisonReason),
}

/// Decode `bytes` into a verified [`SolveJob`], enforcing the size bound first.
///
/// The size gate runs *before* [`decode`](crate::bus::Wire::decode): a buffer
/// larger than [`ValidateConfig::max_decoded_bytes`] is refused as
/// [`PoisonReason::Oversized`] without ever being handed to the decoder, so a
/// corrupt or hostile length prefix cannot drive an unbounded allocation. A
/// buffer that passes the gate is decoded and self-verified by
/// [`SolveJob::decode_verified`], whose failures (undecodable bytes, a forged
/// id, a foreign schema) map onto the matching [`PoisonReason`].
///
/// # Errors
///
/// Returns a [`PoisonReason`] when the buffer is over the bound, does not
/// decode, or decodes to an envelope that fails self-verification.
pub fn check_bytes<E>(bytes: &[u8], cfg: &ValidateConfig) -> Result<SolveJob<E>, PoisonReason>
where
    E: Entry + serde::de::DeserializeOwned,
{
    if bytes.len() > cfg.max_decoded_bytes {
        return Err(PoisonReason::Oversized {
            bytes: bytes.len(),
            limit: cfg.max_decoded_bytes,
        });
    }
    Ok(SolveJob::decode_verified(bytes)?)
}

/// Decide whether an already-decoded `job` should be solved, refused with a
/// typed outcome, or (never here) poisoned.
///
/// `region` and `cells` are the two pieces of the matcher's loaded graph this
/// decision needs — the region the replica serves and the set of cells it can
/// solve over — passed directly (the caller hands `&loaded.region` and
/// `&loaded.cells`) so the function stays pure and needs no live network.
///
/// The checks run in a fixed order, each short-circuiting to a
/// [`Refuse`](Checked::Refuse):
///
/// 1. **Graph.** A job cut against a different graph than this region serves
///    would solve against the wrong network. Refused as
///    [`SolveOutcome::VersionMismatch`] (`expected` = this region's graph).
/// 2. **Region.** A job addressed to another region reaching this consumer is a
///    topology bug — a mis-provisioned stream or consumer filter — not a normal
///    outcome. It is refused as [`SolveOutcome::Internal`] rather than dropped
///    silently so the owner is told its routing is wrong, and rather than
///    [`SolveOutcome::VersionMismatch`] which would misattribute a routing fault
///    to a graph skew.
/// 3. **Coverage.** The head observation's cell must be one this region serves,
///    or the solve has no network under it; refused as
///    [`SolveOutcome::UnsupportedCoverage`]. A job with no head observation
///    cannot be solved or coverage-checked and is refused as
///    [`SolveOutcome::Internal`].
/// 4. **Resume coverage (note only).** For a [`Continuation::Resume`], a trip
///    origin whose cell this region does not serve would be dropped by the
///    solver's own downgrade anyway, so it is *not* a refusal — the job still
///    solves — but it is noted, because a resume that keeps straying outside
///    coverage points at a mis-sized region.
/// 5. **Deadline.** A job with no time left, or less than
///    [`ValidateConfig::min_remaining`], is refused as
///    [`SolveOutcome::DeadlineExpired`]: starting a solve that cannot finish
///    before its answer is useless only wastes the CPU the queue is short of.
#[must_use]
pub fn check<'j, E>(
    job: &'j SolveJob<E>,
    region: &Region,
    cells: &HashSet<Geohash>,
    now_us: i64,
    cfg: &ValidateConfig,
) -> Checked<'j, E>
where
    E: Entry,
{
    // 1. Graph: solving against the wrong network is never acceptable.
    if job.identity.graph != region.graph {
        return Checked::Refuse(SolveOutcome::VersionMismatch {
            expected: region.graph.clone(),
            got: job.identity.graph.clone(),
        });
    }

    // 2. Region: a foreign-region job here is a routing/topology fault, surfaced
    //    to the owner as Internal rather than silently dropped.
    if job.identity.region != region.id {
        return Checked::Refuse(SolveOutcome::Internal {
            reason: format!(
                "region mismatch: job for region {} reached a matcher serving region {}",
                job.identity.region, region.id
            ),
        });
    }

    // 3. Coverage of the head observation — the position this job is solving for.
    let Some(head) = job.head() else {
        return Checked::Refuse(SolveOutcome::Internal {
            reason: "job has no head observation to solve".to_owned(),
        });
    };
    let head_cell = event::shard_of(head.point);
    if !cells.contains(&head_cell) {
        return Checked::Refuse(SolveOutcome::UnsupportedCoverage {
            cell: head_cell.to_string(),
        });
    }

    // 4. Resume history that strays outside coverage: note it, but still solve —
    //    the solver downgrades those layers itself, so refusing here would drop
    //    a job the engine would have made partial progress on.
    if let Continuation::Resume { trip, .. } = &job.context {
        let strayed = unserved_origins(trip.origins(), cells);
        if strayed > 0 {
            tracing::debug!(
                job = %job.id,
                strayed,
                total = trip.origins().len(),
                "resume trip has origins outside loaded coverage; solving anyway",
            );
        }
    }

    // 5. Deadline: refuse anything that cannot finish in time to be useful.
    match job.remaining(now_us) {
        None => Checked::Refuse(SolveOutcome::DeadlineExpired),
        Some(remaining) if remaining < cfg.min_remaining => {
            Checked::Refuse(SolveOutcome::DeadlineExpired)
        }
        Some(_) => Checked::Solve(job),
    }
}

/// How many of `origins` fall on a cell that `cells` does not serve. Used for
/// the resume-coverage note in [`check`]; factored out so the (network-free)
/// counting can be tested directly against a hand-built origin slice.
fn unserved_origins(origins: &[Origin], cells: &HashSet<Geohash>) -> usize {
    origins
        .iter()
        .filter(|origin| !cells.contains(&event::shard_of(origin.point)))
        .count()
}

#[cfg(test)]
mod tests {
    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_transition::matcher::Trip;

    use super::*;
    use crate::bus::Wire;
    use crate::event::VehicleId;
    use crate::protocol::ids::{
        GraphVersion, JobId, Lane, ObservationId, RegionId, SCHEMA_VERSION,
    };
    use crate::protocol::job::JobIdentity;
    use crate::region::Replicas;

    const GRAPH: &str = "test-graph";
    const REGION: &str = "test-region";

    /// A point far enough inland to sit squarely inside one shard cell, plus the
    /// cell `shard_of` maps it to, so a test can serve (or withhold) exactly it.
    fn head_point() -> Point {
        Point::new(151.2093, -33.8688)
    }

    fn served_cell() -> Geohash {
        event::shard_of(head_point())
    }

    fn identity(graph: &str, region: &str) -> JobIdentity {
        JobIdentity {
            schema: SCHEMA_VERSION,
            vehicle_id: VehicleId(1),
            observation: ObservationId {
                partition: 3,
                sequence: 7,
            },
            base: None,
            graph: GraphVersion::new(graph).unwrap(),
            region: RegionId::new(region).unwrap(),
        }
    }

    /// A region serving `GRAPH`/`REGION`. Its `coverage` is irrelevant to
    /// [`check`] (which reads only `graph` and `id`); the served-cell set is
    /// passed separately.
    fn region() -> Region {
        Region {
            id: RegionId::new(REGION).unwrap(),
            graph: GraphVersion::new(GRAPH).unwrap(),
            coverage: vec![served_cell()],
            overlap: Vec::new(),
            lanes: 1,
            resource_class: "cpu-1".to_owned(),
            replicas: Replicas { min: 1, max: 1 },
            freshness_budget: Duration::from_millis(1000),
        }
    }

    /// A cell set containing exactly the head's served cell.
    fn serving_cells() -> HashSet<Geohash> {
        HashSet::from([served_cell()])
    }

    fn restart(head: Point) -> Continuation<MockEntryId> {
        Continuation::Restart {
            fresh: vec![Origin::new(head, 1_000)],
        }
    }

    /// A well-formed restart job over `head`, deadline `deadline_us`.
    fn job(graph: &str, region_id: &str, head: Point, deadline_us: i64) -> SolveJob<MockEntryId> {
        SolveJob::new(
            identity(graph, region_id),
            Lane::DEFAULT,
            deadline_us,
            restart(head),
        )
    }

    /// A job comfortably inside every bound: right graph/region, served head,
    /// a full second of deadline left at `now = 0`.
    fn good_job() -> SolveJob<MockEntryId> {
        job(GRAPH, REGION, head_point(), 1_000_000)
    }

    // --- check_bytes -------------------------------------------------------

    #[test]
    fn check_bytes_refuses_oversize_before_decoding() {
        // An 8-byte bound with a 9-byte buffer: the gate fires without ever
        // touching the decoder, so even nonsense bytes are reported as Oversized.
        let cfg = ValidateConfig {
            max_decoded_bytes: 8,
            ..ValidateConfig::default()
        };
        let bytes = vec![0xAB_u8; 9];
        match check_bytes::<MockEntryId>(&bytes, &cfg) {
            Err(PoisonReason::Oversized { bytes, limit }) => {
                assert_eq!(bytes, 9);
                assert_eq!(limit, 8);
            }
            other => panic!("expected Oversized poison, got {other:?}"),
        }
    }

    #[test]
    fn check_bytes_reports_undecodable_bytes() {
        let cfg = ValidateConfig::default();
        match check_bytes::<MockEntryId>(&[0xff, 0xff, 0xff, 0xff], &cfg) {
            Err(PoisonReason::Decode(_)) => {}
            other => panic!("expected Decode poison, got {other:?}"),
        }
    }

    #[test]
    fn check_bytes_reports_a_tampered_id() {
        let cfg = ValidateConfig::default();
        let mut job = good_job();
        job.id = JobId(job.id.0 ^ 1);
        let bytes = job.encode().unwrap();
        assert!(matches!(
            check_bytes::<MockEntryId>(&bytes, &cfg),
            Err(PoisonReason::IdMismatch)
        ));
    }

    #[test]
    fn check_bytes_reports_a_foreign_schema() {
        let cfg = ValidateConfig::default();
        // Built with schema 2: `id` matches its identity (so it is not an id
        // mismatch), but the schema check inside `verify` fails.
        let mut ident = identity(GRAPH, REGION);
        ident.schema = SchemaVersion(2);
        let job =
            SolveJob::<MockEntryId>::new(ident, Lane::DEFAULT, 1_000_000, restart(head_point()));
        let bytes = job.encode().unwrap();
        match check_bytes::<MockEntryId>(&bytes, &cfg) {
            Err(PoisonReason::Schema { got }) => assert_eq!(got, SchemaVersion(2)),
            other => panic!("expected Schema poison, got {other:?}"),
        }
    }

    #[test]
    fn check_bytes_accepts_a_well_formed_job() {
        let cfg = ValidateConfig::default();
        let job = good_job();
        let bytes = job.encode().unwrap();
        let decoded = check_bytes::<MockEntryId>(&bytes, &cfg).expect("well-formed job decodes");
        assert_eq!(decoded.id, job.id);
        assert_eq!(decoded.identity, job.identity);
    }

    // --- check: refusals ---------------------------------------------------

    #[test]
    fn check_refuses_a_foreign_graph() {
        let job = job(GRAPH, REGION, head_point(), 1_000_000);
        let region = Region {
            graph: GraphVersion::new("other-graph").unwrap(),
            ..region()
        };
        match check(
            &job,
            &region,
            &serving_cells(),
            0,
            &ValidateConfig::default(),
        ) {
            Checked::Refuse(SolveOutcome::VersionMismatch { expected, got }) => {
                assert_eq!(expected, GraphVersion::new("other-graph").unwrap());
                assert_eq!(got, GraphVersion::new(GRAPH).unwrap());
            }
            other => panic!("expected VersionMismatch refusal, got {other:?}"),
        }
    }

    #[test]
    fn check_refuses_a_foreign_region_as_internal() {
        // The job is addressed to a different region than this matcher serves.
        let job = job(GRAPH, "other-region", head_point(), 1_000_000);
        match check(
            &job,
            &region(),
            &serving_cells(),
            0,
            &ValidateConfig::default(),
        ) {
            Checked::Refuse(SolveOutcome::Internal { reason }) => {
                assert!(reason.starts_with("region mismatch"), "reason: {reason}");
            }
            other => panic!("expected Internal refusal, got {other:?}"),
        }
    }

    #[test]
    fn check_refuses_an_unserved_head_cell() {
        // The head is served nowhere in the (empty) cell set.
        let job = good_job();
        let empty = HashSet::new();
        match check(&job, &region(), &empty, 0, &ValidateConfig::default()) {
            Checked::Refuse(SolveOutcome::UnsupportedCoverage { cell }) => {
                assert_eq!(cell, served_cell().to_string());
            }
            other => panic!("expected UnsupportedCoverage refusal, got {other:?}"),
        }
    }

    #[test]
    fn check_refuses_a_headless_job_as_internal() {
        // A restart with no fresh origins has no head to solve or locate.
        let job = SolveJob::<MockEntryId>::new(
            identity(GRAPH, REGION),
            Lane::DEFAULT,
            1_000_000,
            Continuation::Restart { fresh: Vec::new() },
        );
        match check(
            &job,
            &region(),
            &serving_cells(),
            0,
            &ValidateConfig::default(),
        ) {
            Checked::Refuse(SolveOutcome::Internal { reason }) => {
                assert!(reason.contains("head observation"), "reason: {reason}");
            }
            other => panic!("expected Internal refusal, got {other:?}"),
        }
    }

    #[test]
    fn check_refuses_an_expired_deadline() {
        // Deadline exactly at `now`: nothing remains.
        let job = job(GRAPH, REGION, head_point(), 0);
        assert!(matches!(
            check(
                &job,
                &region(),
                &serving_cells(),
                0,
                &ValidateConfig::default()
            ),
            Checked::Refuse(SolveOutcome::DeadlineExpired)
        ));
    }

    #[test]
    fn check_refuses_a_deadline_too_close_to_start() {
        // 100 ms left, but the floor is 250 ms: not worth starting.
        let cfg = ValidateConfig::default();
        let job = job(GRAPH, REGION, head_point(), 100_000);
        assert!(matches!(
            check(&job, &region(), &serving_cells(), 0, &cfg),
            Checked::Refuse(SolveOutcome::DeadlineExpired)
        ));
    }

    // --- check: acceptance & ordering -------------------------------------

    #[test]
    fn check_solves_a_well_formed_restart() {
        let job = good_job();
        match check(
            &job,
            &region(),
            &serving_cells(),
            0,
            &ValidateConfig::default(),
        ) {
            Checked::Solve(solved) => assert_eq!(solved.id, job.id),
            other => panic!("expected Solve, got {other:?}"),
        }
    }

    #[test]
    fn check_solves_a_resume_with_an_empty_trip() {
        // A resume whose history is empty passes coverage on the head alone.
        let context = Continuation::<MockEntryId>::Resume {
            trip: Trip::new(),
            fresh: vec![Origin::new(head_point(), 1_000)],
        };
        let job = SolveJob::new(identity(GRAPH, REGION), Lane::DEFAULT, 1_000_000, context);
        assert!(matches!(
            check(
                &job,
                &region(),
                &serving_cells(),
                0,
                &ValidateConfig::default()
            ),
            Checked::Solve(_)
        ));
    }

    #[test]
    fn check_prefers_the_graph_refusal_over_a_later_failure() {
        // A job that is *also* expired and *also* out of coverage still reports
        // the graph mismatch, proving the checks run in the documented order.
        let job = job("other-graph", REGION, head_point(), 0);
        let empty = HashSet::new();
        assert!(matches!(
            check(&job, &region(), &empty, 0, &ValidateConfig::default()),
            Checked::Refuse(SolveOutcome::VersionMismatch { .. })
        ));
    }

    #[test]
    fn check_prefers_coverage_over_the_deadline() {
        // Out-of-coverage *and* expired: coverage is checked first (§ order).
        let job = job(GRAPH, REGION, head_point(), 0);
        let empty = HashSet::new();
        assert!(matches!(
            check(&job, &region(), &empty, 0, &ValidateConfig::default()),
            Checked::Refuse(SolveOutcome::UnsupportedCoverage { .. })
        ));
    }

    // --- unserved_origins helper ------------------------------------------

    #[test]
    fn unserved_origins_counts_only_the_strays() {
        let served = served_cell();
        let cells = HashSet::from([served]);
        // Two origins on the served cell, one far away on another cell.
        let elsewhere = Point::new(2.3522, 48.8566); // Paris: a different shard
        assert_ne!(event::shard_of(elsewhere), served, "fixture points differ");
        let origins = [
            Origin::new(head_point(), 1),
            Origin::new(head_point(), 2),
            Origin::new(elsewhere, 3),
        ];
        assert_eq!(unserved_origins(&origins, &cells), 1);
        assert_eq!(unserved_origins(&origins, &HashSet::new()), 3);
        assert_eq!(unserved_origins(&[], &cells), 0);
    }
}
