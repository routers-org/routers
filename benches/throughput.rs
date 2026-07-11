//! Map-matching throughput (points/sec) over real-world traces.
//!
//! Compares the streaming workflow — where the trellis acts as a cache and
//! the Viterbi forward pass resumes — against the naive online approaches
//! that predate `routers_trellis`:
//!
//!  - `streaming` — push per point, `MatchState` retained: only the new
//!    boundary is weighed and the forward pass resumes. O(n) total.
//!  - `naive`     — identical incremental generation (push per point), but a
//!    fresh `MatchState` per arriving point: the trellis cache and forward
//!    pass are thrown away and every boundary is re-weighed, every step.
//!    O(n²) total — the delta against `streaming` is exactly the
//!    trellis-as-cache win.
//!  - `naive:window20` — the classic sliding-window online matcher: per point,
//!    rebuild and solve a transition over only the last 20 points. Bounded
//!    per-step cost, but approximate (no memory past the window) and it redoes
//!    the window's layer generation, as a real windowed matcher would.
//!
//! Every trace is culled to its last [`TAIL_LEN`] points so all strategies —
//! including the quadratic naive baseline — run over the same bounded, recent
//! history, and iterations stay short enough for a proper sample count.
//! Every benchmark reports `Throughput::Elements`, so criterion prints
//! points/sec directly.
//!
//! Two groups:
//!  - `throughput` — a single vehicle (one trace from `SYDNEY_THROUGHPUT_TRIP`):
//!    the per-session rate, three-way head-to-head.
//!  - `throughput:fleet` — 64 vehicles matched concurrently (rayon, one
//!    session per trace, `SYDNEY_THROUGHPUT_FLEET`): the fully-saturated
//!    aggregate points/sec a host of this machine's core count can sustain.
//!
//! Warm benches share one `PredicateCache` (primed before measuring); cold
//! benches get a fresh cache per iteration (allocated in setup, excluded from
//! the measurement) — the realtime first-match cost.

use std::sync::Arc;

use criterion::{BatchSize, Throughput, black_box, criterion_main};
use geo::{LineString, Point};
use rayon::prelude::*;
use wkt::TryFromWkt;

use routers::DEFAULT_SEARCH_DISTANCE;
use routers::generation::StandardGenerator;
use routers::transition::*;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_fixtures::{SYDNEY, SYDNEY_THROUGHPUT_FLEET, SYDNEY_THROUGHPUT_TRIP, fixture_path};
use routers_network::Metadata;

type BenchSolver = AllComputeSolver<OsmEntryId, OsmEdgeMetadata, OsmNetwork>;
type BenchCache = PredicateCache<OsmEntryId, OsmEdgeMetadata, OsmNetwork>;

/// Every strategy sees only the last `TAIL_LEN` points of each trace: the
/// bounded recent history a realtime session cares about, and short enough
/// that the O(n²) naive baseline completes in tractable time.
const TAIL_LEN: usize = 100;

/// Sliding-window size for the windowed baseline.
const WINDOW: usize = 20;

struct Harness {
    graph: OsmNetwork,
    strategies: CostingStrategies<DefaultEmissionCost, DefaultTransitionCost, OsmEntryId>,
    runtime: <OsmEdgeMetadata as Metadata>::Runtime,
}

impl Harness {
    fn generator(&self) -> StandardGenerator<'_, OsmEntryId, OsmEdgeMetadata, DefaultEmissionCost> {
        StandardGenerator::new(
            &self.graph,
            &self.strategies.emission,
            DEFAULT_SEARCH_DISTANCE,
        )
    }

    /// Keep only anchorable points (`push` rejects a point it cannot anchor
    /// without mutating the transition), then cull to the last [`TAIL_LEN`].
    fn cull(&self, trace: LineString<f64>) -> Vec<Point> {
        let generator = self.generator();
        let mut probe = Transition::empty(&self.graph, &self.strategies);
        let anchored: Vec<Point> = trace
            .into_points()
            .into_iter()
            .filter(|p| probe.push(*p, &generator).is_ok())
            .collect();

        anchored[anchored.len().saturating_sub(TAIL_LEN)..].to_vec()
    }

    /// The streaming loop: each point is pushed as it "arrives", the state
    /// extends, and the solve resumes off the retained trellis.
    fn stream(&self, points: &[Point], solver: &BenchSolver) {
        let generator = self.generator();
        let mut transition = Transition::empty(&self.graph, &self.strategies);
        let mut state = MatchState::default();

        for point in points {
            let width = transition
                .push(*point, &generator)
                .expect("point must anchor");
            state.extend(width).expect("state must extend");
            solver
                .solve_path(&transition, &self.runtime, &mut state)
                .expect("streaming solve must succeed");
        }

        black_box(&state);
    }

    /// Whether a full streaming pass over `points` anchors and solves cleanly.
    /// Some real-world tails cross disconnections in the (minified) graph and
    /// cannot be matched; every strategy shares those boundaries, so streaming
    /// success qualifies a trace for all of them.
    fn streamable(&self, points: &[Point], solver: &BenchSolver) -> bool {
        let generator = self.generator();
        let mut transition = Transition::empty(&self.graph, &self.strategies);
        let mut state = MatchState::default();

        points.iter().all(|point| {
            transition.push(*point, &generator).is_ok_and(|width| {
                state.extend(width).is_ok()
                    && solver
                        .solve_path(&transition, &self.runtime, &mut state)
                        .is_ok()
            })
        })
    }

    /// The naive realtime baseline: same incremental generation, but a fresh
    /// MatchState per point discards all weighed boundaries and the cached
    /// forward pass, redoing the whole trellis every time.
    fn stream_naive(&self, points: &[Point], solver: &BenchSolver) {
        let generator = self.generator();
        let mut transition = Transition::empty(&self.graph, &self.strategies);

        for point in points {
            transition
                .push(*point, &generator)
                .expect("point must anchor");

            let mut state = MatchState::default();
            for width in transition.widths() {
                state.extend(width).expect("state must extend");
            }
            solver
                .solve_path(&transition, &self.runtime, &mut state)
                .expect("naive solve must succeed");
        }
    }

    /// The windowed naive baseline: per point, rebuild and solve a transition
    /// over only the last [`WINDOW`] points.
    fn stream_windowed(&self, points: &[Point], solver: &BenchSolver) {
        let mut seen: Vec<Point> = Vec::new();

        for point in points {
            seen.push(*point);
            let tail = seen.len().saturating_sub(WINDOW);

            let transition = Transition::new(
                &self.graph,
                LineString::from(seen[tail..].to_vec()),
                &self.strategies,
                self.generator(),
            );
            solver
                .solve(transition, &self.runtime, &mut MatchState::default())
                .expect("windowed solve must succeed");
        }
    }
}

fn fresh_cache() -> Arc<BenchCache> {
    Arc::new(BenchCache::default())
}

fn throughput_benchmark(c: &mut criterion::Criterion) {
    let harness = Harness {
        graph: OsmNetwork::from_pbf(&fixture_path(SYDNEY)).expect("Graph must be created"),
        strategies: CostingStrategies::default(),
        runtime: OsmEdgeMetadata::default_runtime(),
    };

    // == Single vehicle: per-session rate ==

    let wkt = std::fs::read_to_string(fixture_path(SYDNEY_THROUGHPUT_TRIP))
        .expect("Trace fixture must be readable");
    let trace = LineString::try_from_wkt_str(&wkt).expect("Linestring must parse successfully.");
    let points = harness.cull(trace);
    assert_eq!(points.len(), TAIL_LEN, "trace tail must anchor fully");

    // One shared warm cache, primed before measuring.
    let cache = fresh_cache();
    harness.stream(&points, &BenchSolver::default().use_cache(cache.clone()));

    let mut group = c.benchmark_group("throughput");
    group
        .significance_level(0.1)
        .sample_size(30)
        .measurement_time(core::time::Duration::from_secs(20));
    group.throughput(Throughput::Elements(points.len() as u64));

    group.bench_function(format!("streaming: {TAIL_LEN}pts"), |b| {
        b.iter(|| harness.stream(&points, &BenchSolver::default().use_cache(cache.clone())))
    });

    group.bench_function(format!("naive: {TAIL_LEN}pts"), |b| {
        b.iter(|| harness.stream_naive(&points, &BenchSolver::default().use_cache(cache.clone())))
    });

    group.bench_function(format!("naive:window{WINDOW}: {TAIL_LEN}pts"), |b| {
        b.iter(|| {
            harness.stream_windowed(&points, &BenchSolver::default().use_cache(cache.clone()))
        })
    });

    group.bench_function(format!("streaming:cold: {TAIL_LEN}pts"), |b| {
        b.iter_batched(
            fresh_cache,
            |cold| harness.stream(&points, &BenchSolver::default().use_cache(cold)),
            BatchSize::SmallInput,
        )
    });

    group.bench_function(format!("naive:cold: {TAIL_LEN}pts"), |b| {
        b.iter_batched(
            fresh_cache,
            |cold| harness.stream_naive(&points, &BenchSolver::default().use_cache(cold)),
            BatchSize::SmallInput,
        )
    });

    group.bench_function(format!("naive:window{WINDOW}:cold: {TAIL_LEN}pts"), |b| {
        b.iter_batched(
            fresh_cache,
            |cold| harness.stream_windowed(&points, &BenchSolver::default().use_cache(cold)),
            BatchSize::SmallInput,
        )
    });

    group.finish();

    // == Fleet: fully-saturated aggregate rate ==
    //
    // 64 independent vehicle sessions matched concurrently (one rayon task
    // per trace, each with its own Transition/MatchState, all sharing the
    // one PredicateCache — exactly a multi-vehicle realtime host). Aggregate
    // Throughput::Elements = total points across the fleet, so criterion
    // reports the saturated machine-wide points/sec.

    let candidates: Vec<Vec<Point>> =
        std::fs::read_to_string(fixture_path(SYDNEY_THROUGHPUT_FLEET))
            .expect("Fleet fixture must be readable")
            .lines()
            .map(|line| {
                let trace = LineString::try_from_wkt_str(line)
                    .expect("Linestring must parse successfully.");
                harness.cull(trace)
            })
            .filter(|points| !points.is_empty())
            .collect();

    // Qualify traces (dropping any that hit a graph disconnection) while
    // simultaneously priming the shared warm cache over the survivors.
    let cache = fresh_cache();
    let total = candidates.len();
    let fleet: Vec<Vec<Point>> = candidates
        .into_par_iter()
        .filter(|points| {
            harness.streamable(points, &BenchSolver::default().use_cache(cache.clone()))
        })
        .collect();
    let fleet_points: usize = fleet.iter().map(Vec::len).sum();
    eprintln!(
        "fleet: {}/{total} traces matchable ({fleet_points} points)",
        fleet.len()
    );

    let mut group = c.benchmark_group("throughput:fleet");
    group
        .significance_level(0.1)
        .sample_size(10)
        .measurement_time(core::time::Duration::from_secs(30));
    group.throughput(Throughput::Elements(fleet_points as u64));

    let vehicles = fleet.len();

    group.bench_function(format!("streaming: {vehicles}x{TAIL_LEN}pts"), |b| {
        b.iter(|| {
            fleet.par_iter().for_each(|points| {
                harness.stream(points, &BenchSolver::default().use_cache(cache.clone()))
            })
        })
    });

    group.bench_function(format!("naive: {vehicles}x{TAIL_LEN}pts"), |b| {
        b.iter(|| {
            fleet.par_iter().for_each(|points| {
                harness.stream_naive(points, &BenchSolver::default().use_cache(cache.clone()))
            })
        })
    });

    group.bench_function(
        format!("naive:window{WINDOW}: {vehicles}x{TAIL_LEN}pts"),
        |b| {
            b.iter(|| {
                fleet.par_iter().for_each(|points| {
                    harness
                        .stream_windowed(points, &BenchSolver::default().use_cache(cache.clone()))
                })
            })
        },
    );

    group.bench_function(format!("streaming:cold: {vehicles}x{TAIL_LEN}pts"), |b| {
        b.iter_batched(
            fresh_cache,
            |cold| {
                fleet.par_iter().for_each(|points| {
                    harness.stream(points, &BenchSolver::default().use_cache(cold.clone()))
                })
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function(format!("naive:cold: {vehicles}x{TAIL_LEN}pts"), |b| {
        b.iter_batched(
            fresh_cache,
            |cold| {
                fleet.par_iter().for_each(|points| {
                    harness.stream_naive(points, &BenchSolver::default().use_cache(cold.clone()))
                })
            },
            BatchSize::SmallInput,
        )
    });

    group.bench_function(
        format!("naive:window{WINDOW}:cold: {vehicles}x{TAIL_LEN}pts"),
        |b| {
            b.iter_batched(
                fresh_cache,
                |cold| {
                    fleet.par_iter().for_each(|points| {
                        harness.stream_windowed(
                            points,
                            &BenchSolver::default().use_cache(cold.clone()),
                        )
                    })
                },
                BatchSize::SmallInput,
            )
        },
    );

    group.finish();
}

criterion::criterion_group!(throughput_benches, throughput_benchmark);
criterion_main!(throughput_benches);
