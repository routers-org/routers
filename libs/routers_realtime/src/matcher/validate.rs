//! Matcher job validator: classify one pulled [`SolveJob`] into solve, refuse,
//! or poison, so a matcher never solves work it cannot answer correctly.
//!
//! An invalid job yields a typed unsuccessful result, never silence. Wire-level
//! decode ([`check_bytes`]) is split from the semantic checks ([`check`]) so
//! [`check`] stays a pure, network-free function.

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

/// How strict the validator is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidateConfig {
    /// The largest wire buffer the matcher will decode, enforced before decoding.
    pub max_decoded_bytes: usize,
}

impl ValidateConfig {
    /// The default decode bound: 4 MiB.
    pub const DEFAULT_MAX_DECODED_BYTES: usize = 4 << 20;
}

impl Default for ValidateConfig {
    fn default() -> Self {
        Self {
            max_decoded_bytes: Self::DEFAULT_MAX_DECODED_BYTES,
        }
    }
}

/// Why a job's bytes could not be turned into a result-bearing job at all.
///
/// A poison job is acknowledged and dropped, never answered: there is no
/// trustworthy identity to address a result to.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum PoisonReason {
    /// The buffer did not decode into a [`SolveJob`] (truncated or corrupt).
    #[error("could not decode solve job: {0}")]
    Decode(String),
    /// The envelope's `id` was not the digest of its identity — forged in flight.
    #[error("job id does not match its identity")]
    IdMismatch,
    /// The job was produced against a wire schema this build does not speak.
    #[error("job was produced against schema {got}, which this build does not speak")]
    Schema { got: SchemaVersion },
    /// The raw buffer exceeded [`ValidateConfig::max_decoded_bytes`], rejected before decoding.
    #[error("job envelope is {bytes} bytes, over the {limit}-byte decode bound")]
    Oversized { bytes: usize, limit: usize },
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
/// case so the caller hands the very same value to the engine.
#[derive(Debug)]
pub enum Checked<'j, E: Entry> {
    /// The job passed every check: solve it.
    Solve(&'j SolveJob<E>),
    /// The job must not be solved; publish this typed outcome as its result.
    Refuse(SolveOutcome<E>),
    /// The bytes could not be turned into an answerable job; ack and drop.
    Poison(PoisonReason),
}

/// Decode `bytes` into a verified [`SolveJob`], rejecting a buffer over
/// [`ValidateConfig::max_decoded_bytes`] before decoding so a hostile length
/// prefix cannot drive an unbounded allocation.
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

/// Decide whether an already-decoded `job` should be solved or refused with a
/// typed outcome. `region` and `cells` are passed directly so the function stays
/// pure and network-free; the checks short-circuit in the order graph → region →
/// coverage. The authenticated freshness target is intentionally not an
/// admission criterion: a delayed, replayed, or clock-skewed observation still
/// has valid per-vehicle ordering and must be solved.
#[must_use]
pub fn check<'j, E>(
    job: &'j SolveJob<E>,
    region: &Region,
    cells: &HashSet<Geohash>,
    now_us: i64,
    _cfg: &ValidateConfig,
) -> Checked<'j, E>
where
    E: Entry,
{
    if job.identity.graph != region.graph {
        return Checked::Refuse(SolveOutcome::VersionMismatch {
            expected: region.graph.clone(),
            got: job.identity.graph.clone(),
        });
    }

    if job.identity.region != region.id {
        return Checked::Refuse(SolveOutcome::Internal {
            reason: format!(
                "region mismatch: job for region {} reached a matcher serving region {}",
                job.identity.region, region.id
            ),
        });
    }

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

    // Strayed resume origins are noted, not refused: the solver downgrades them itself.
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

    let _ = now_us;
    Checked::Solve(job)
}

/// How many of `origins` fall on a cell that `cells` does not serve.
fn unserved_origins(origins: &[Origin], cells: &HashSet<Geohash>) -> usize {
    origins
        .iter()
        .filter(|origin| !cells.contains(&event::shard_of(origin.point)))
        .count()
}

#[cfg(test)]
mod tests {
    use core::time::Duration;

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
    use core::num::{NonZeroU8, NonZeroU32};

    use crate::region::{LaneCount, Replicas};

    const GRAPH: &str = "test-graph";
    const REGION: &str = "test-region";

    /// A point far inland, squarely inside one shard cell.
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

    fn region() -> Region {
        Region {
            id: RegionId::new(REGION).unwrap(),
            graph: GraphVersion::new(GRAPH).unwrap(),
            coverage: vec![served_cell()],
            overlap: Vec::new(),
            lanes: LaneCount::new(NonZeroU8::MIN),
            resource_class: "cpu-1".to_owned(),
            replicas: Replicas::new(NonZeroU32::MIN, NonZeroU32::MIN).expect("range"),
            freshness_budget: Duration::from_millis(1000),
        }
    }

    fn serving_cells() -> HashSet<Geohash> {
        HashSet::from([served_cell()])
    }

    fn restart(head: Point) -> Continuation<MockEntryId> {
        Continuation::Restart {
            fresh: vec![Origin::new(head, 1_000)],
        }
    }

    fn job(graph: &str, region_id: &str, head: Point, target_us: i64) -> SolveJob<MockEntryId> {
        SolveJob::new(
            identity(graph, region_id),
            Lane::DEFAULT,
            target_us,
            restart(head),
        )
    }

    fn good_job() -> SolveJob<MockEntryId> {
        job(GRAPH, REGION, head_point(), 1_000_000)
    }

    // --- check_bytes -------------------------------------------------------

    #[test]
    fn check_bytes_refuses_oversize_before_decoding() {
        let cfg = ValidateConfig {
            max_decoded_bytes: 8,
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
        // Schema 2 so `id` still matches its identity; only the schema check fails.
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
    fn check_solves_an_overdue_freshness_target() {
        let job = job(GRAPH, REGION, head_point(), 0);
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
    fn check_solves_a_freshness_target_too_close_to_start() {
        let cfg = ValidateConfig::default();
        let job = job(GRAPH, REGION, head_point(), 100_000);
        assert!(matches!(
            check(&job, &region(), &serving_cells(), 0, &cfg),
            Checked::Solve(_)
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
        // Also expired and out of coverage, yet still reports the graph mismatch.
        let job = job("other-graph", REGION, head_point(), 0);
        let empty = HashSet::new();
        assert!(matches!(
            check(&job, &region(), &empty, 0, &ValidateConfig::default()),
            Checked::Refuse(SolveOutcome::VersionMismatch { .. })
        ));
    }

    #[test]
    fn check_prefers_coverage_over_an_overdue_freshness_target() {
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
