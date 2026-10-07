//! Solve determinism on the real Sydney graph.
//!
//! Replay after a crash, direct commits and sticky matchers all assume a solve's
//! result depends only on its job. Each vehicle's job chain is solved on an
//! engine with production caches (vehicles in turn) and one with thrashing
//! 64-entry caches (vehicles interleaved), and a cold engine re-solves a sample.
//! Every outcome, trip digest included, must be byte-identical.
//!
//! Run with the local shard cache and dataset:
//!
//! ```sh
//! cargo test -p routers_realtime --test determinism -- --ignored --nocapture
//! ```
//!
//! `ROUTERS_TEST_SHARD_DIR`, `ROUTERS_TEST_CSV` and `ROUTERS_TEST_ROWS` override
//! the defaults (the workspace shard cache, the Sydney dump, 20,000 rows).

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use std::path::{Path, PathBuf};

use chrono::NaiveDateTime;
use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_network::Metadata;
use routers_realtime::event::VehicleId;
use routers_realtime::lifecycle::Readiness;
use routers_realtime::matcher::bootstrap::{BootstrapConfig, bootstrap};
use routers_realtime::matcher::engine::Engine;
use routers_realtime::protocol::ids::{
    GraphVersion, Lane, ObservationId, RegionId, Revision, SCHEMA_VERSION, SegmentId,
};
use routers_realtime::protocol::job::{BaseState, JobIdentity, SolveJob, TripDigest};
use routers_realtime::protocol::result::SolveOutcome;
use routers_transition::matcher::{Continuation, Origin, Trip};
use web_time::Instant;

type E = OsmEntryId;

const REGION: &str = "sydney";
const GRAPH: &str = "sydney-local";
const CELLS: [&str; 6] = ["r3gq", "r3gr", "r3gw", "r3gx", "r652", "r658"];

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every vehicle's observations in event-time order, from the first `rows`
/// rows of the time-sorted dump.
fn vehicle_traces(csv: &Path, rows: usize) -> BTreeMap<u64, Vec<Origin>> {
    let text = std::fs::read_to_string(csv).expect("read the replay CSV");
    let mut events: Vec<(i64, String, Point)> = text
        .lines()
        .skip(1)
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').collect();
            let time = NaiveDateTime::parse_from_str(fields.get(3)?, "%Y-%m-%d %H:%M:%S UTC")
                .ok()?
                .and_utc()
                .timestamp_micros();
            let lat: f64 = fields.get(4)?.parse().ok()?;
            let lon: f64 = fields.get(5)?.parse().ok()?;
            Some((time, fields.get(1)?.to_string(), Point::new(lon, lat)))
        })
        .collect();
    events.sort_by_key(|(time, _, _)| *time);
    let mut traces: BTreeMap<u64, Vec<Origin>> = BTreeMap::new();
    let mut ids: BTreeMap<String, u64> = BTreeMap::new();
    for (time, vehicle, point) in events.into_iter().take(rows) {
        let next = ids.len() as u64 + 1;
        let id = *ids.entry(vehicle).or_insert(next);
        let trace = traces.entry(id).or_default();
        // A repeated timestamp is a regression the owner would never dispatch.
        if trace.last().is_none_or(|last| last.timestamp < time) {
            trace.push(Origin::new(point, time));
        }
    }
    traces
}

/// The `index`-th job of a vehicle's chain, resuming from `previous`.
fn job(
    vehicle: VehicleId,
    index: usize,
    origin: Origin,
    previous: Option<&(Trip<E>, TripDigest)>,
) -> SolveJob<E> {
    let identity = JobIdentity {
        schema: SCHEMA_VERSION,
        vehicle_id: vehicle,
        observation: ObservationId {
            partition: 0,
            sequence: index as u64 + 1,
        },
        base: previous.map(|_| BaseState {
            revision: Revision(index as u64),
            segment: SegmentId(1),
        }),
        graph: GraphVersion::new(GRAPH).unwrap(),
        region: RegionId::new(REGION).unwrap(),
    };
    match previous {
        Some((trip, digest)) => SolveJob::with_trip_digest(
            identity,
            Lane::DEFAULT,
            i64::MAX,
            Continuation::Resume {
                trip: trip.clone(),
                fresh: vec![origin],
            },
            Some(*digest),
        ),
        None => SolveJob::new(
            identity,
            Lane::DEFAULT,
            i64::MAX,
            Continuation::Restart {
                fresh: vec![origin],
            },
        ),
    }
}

/// One solved step of a chain: the job, the outcome's bytes, and the state the
/// next job resumes from.
struct Step {
    job: SolveJob<E>,
    bytes: Vec<u8>,
    next: Option<(Trip<E>, TripDigest)>,
}

fn solve(engine: &Engine<impl routers_network::Network<Entry = E>>, job: SolveJob<E>) -> Step {
    let outcome = engine.solve(&job);
    let bytes = postcard::to_allocvec(&outcome).expect("outcome encodes");
    let next = match outcome {
        SolveOutcome::Solved {
            trip, trip_digest, ..
        } => {
            let digest = trip_digest.expect("the engine digests its trip");
            assert_eq!(digest, TripDigest::of(&trip), "the digest matches the trip");
            Some((trip, digest))
        }
        // A terminal restarts the chain, as the owner's next job would.
        _ => None,
    };
    Step { job, bytes, next }
}

#[tokio::test]
#[ignore = "needs the local Sydney shard cache and replay CSV"]
// Rounds step every vehicle's chain in lockstep by position; the elapsed time
// is only reported.
#[allow(clippy::needless_range_loop, clippy::disallowed_methods)]
async fn solves_are_byte_identical_across_matcher_histories() {
    let shard_dir = std::env::var_os("ROUTERS_TEST_SHARD_DIR")
        .map_or_else(|| workspace().join("target/shard_cache"), PathBuf::from);
    let csv = std::env::var_os("ROUTERS_TEST_CSV").map_or_else(
        || workspace().join("libs/routers_realtime/data/sydney-dump-2026-thesis.csv"),
        PathBuf::from,
    );
    let rows = std::env::var("ROUTERS_TEST_ROWS")
        .ok()
        .and_then(|rows| rows.parse().ok())
        .unwrap_or(20_000);

    let dir = std::env::temp_dir().join(format!("routers-determinism-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let catalog = dir.join("catalog.toml");
    let coverage = CELLS.map(|cell| format!("\"{cell}\"")).join(", ");
    std::fs::write(
        &catalog,
        format!(
            "version = 1\nrouting_version = 1\n\n[[regions]]\nid = \"{REGION}\"\ngraph = \"{GRAPH}\"\n\
             coverage = [{coverage}]\noverlap = []\nlanes = 1\nresource_class = \"cpu-8\"\n\
             replicas = {{ min = 1, max = 1 }}\nfreshness_budget_ms = 30000\n"
        ),
    )
    .unwrap();
    let (readiness, _watcher) = Readiness::new();
    let loaded = bootstrap(
        &BootstrapConfig {
            catalog,
            region: RegionId::new(REGION).unwrap(),
            shard_dir,
        },
        &readiness,
    )
    .await
    .expect("load the Sydney graph");

    // The matcher binary's configuration, differing only in cache bounds.
    let engine = |predicates: usize, successors: usize| {
        Engine::new(
            Arc::clone(&loaded.network),
            OsmEdgeMetadata::runtime(None),
            None,
        )
        .with_max_candidates(Some(16))
        .with_window_layers(Some(32))
        .with_cache_capacities(predicates, successors)
    };
    let warm = engine(131_072, 131_072);
    let thrashing = engine(64, 64);

    let traces = vehicle_traces(&csv, rows);
    let started = Instant::now();

    // Engine A: one vehicle's whole chain after another.
    let mut chains_a: BTreeMap<u64, Vec<Step>> = BTreeMap::new();
    for (&id, trace) in &traces {
        let vehicle = VehicleId(id);
        let mut previous: Option<(Trip<E>, TripDigest)> = None;
        let chain = chains_a.entry(id).or_default();
        for (index, &origin) in trace.iter().enumerate() {
            let step = solve(&warm, job(vehicle, index, origin, previous.as_ref()));
            previous = step.next.clone();
            chain.push(step);
        }
    }

    // Engine B: every vehicle advanced one step per round, on its own chain.
    let mut previous_b: BTreeMap<u64, Option<(Trip<E>, TripDigest)>> = BTreeMap::new();
    let longest = traces.values().map(Vec::len).max().unwrap_or(0);
    let mut compared = 0usize;
    let mut solved = 0usize;
    for index in 0..longest {
        for (&id, trace) in traces.iter().rev() {
            let vehicle = VehicleId(id);
            let Some(&origin) = trace.get(index) else {
                continue;
            };
            let previous = previous_b.entry(id).or_default();
            let step = solve(&thrashing, job(vehicle, index, origin, previous.as_ref()));
            let reference = &chains_a[&id][index];
            assert_eq!(
                step.job.id, reference.job.id,
                "vehicle {} step {index}: the chains built different jobs",
                vehicle.0
            );
            assert!(
                step.bytes == reference.bytes,
                "vehicle {} step {index}: engines disagree on the same job",
                vehicle.0
            );
            solved += usize::from(step.next.is_some());
            *previous = step.next;
            compared += 1;
        }
    }

    // A cold engine re-solves a sample of engine A's jobs from nothing.
    let cold = engine(131_072, 131_072);
    let mut resolved = 0usize;
    for chain in chains_a.values() {
        for step in chain.iter().step_by(7) {
            let outcome = cold.solve(&step.job);
            assert!(
                postcard::to_allocvec(&outcome).unwrap() == step.bytes,
                "a cold engine disagrees on job {}",
                step.job.id
            );
            resolved += 1;
        }
    }

    eprintln!(
        "determinism: {} vehicles, {compared} jobs compared across histories ({solved} solved), \
         {resolved} re-solved cold, all byte-identical in {:.1}s",
        traces.len(),
        started.elapsed().as_secs_f64()
    );
    assert!(compared > 1_000, "the sample is too small to mean anything");
    let _ = std::fs::remove_dir_all(dir);
}
