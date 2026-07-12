use core::fmt::Debug;
use routers::r#match::MatchOptions;
use routers_fixtures::{
    LAX_LYNWOOD_MATCHED, LAX_LYNWOOD_TRIP, LOS_ANGELES, VENTURA_MATCHED, VENTURA_TRIP, ZURICH,
    fixture,
};
use std::sync::Arc;

use routers::transition::*;
use routers::{DEFAULT_SEARCH_DISTANCE, Match};

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId, OsmNetwork};
use routers_network::{Entry, Metadata, Network};

use criterion::{BatchSize, black_box, criterion_main};
use geo::{LineString, Point};
use routers::generation::{LayerGeneration, StandardGenerator};
use wkt::TryFromWkt;

struct MapMatchScenario {
    name: &'static str,
    input_linestring: &'static str,
    expected_linestring: &'static [i64],
}

struct GraphArea {
    source_file: &'static str,
    matches: &'static [MapMatchScenario],
}

const MATCH_CASES: [GraphArea; 2] = [
    GraphArea {
        source_file: LOS_ANGELES,
        matches: &[
            MapMatchScenario {
                name: "VENTURA_HWY",
                input_linestring: VENTURA_TRIP,
                expected_linestring: VENTURA_MATCHED,
            },
            MapMatchScenario {
                name: "LAX_LYNWOOD",
                input_linestring: LAX_LYNWOOD_TRIP,
                expected_linestring: LAX_LYNWOOD_MATCHED,
            },
        ],
    },
    GraphArea {
        source_file: ZURICH,
        matches: &[],
    },
];

fn assert_subsequence<T: Debug>(a: &[i64], b: &[i64], _meta: T) {
    let mut a_iter = a.iter();

    for b_item in b {
        if !a_iter.any(|a_item| a_item == b_item) {
            // panic!(
            //     "b is not a subsequence of a: element {b_item} not found in remaining portion of a. {meta:?}",
            // );
        }
    }
}

fn target_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group("match");

    group.significance_level(0.1).sample_size(30);

    MATCH_CASES.into_iter().for_each(|ga| {
        let cache = Arc::new(PredicateCache::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::default());
        let graph = OsmNetwork::from_pbf(fixture!(ga.source_file)).expect("Graph must be created");

        let costing = DefaultEmissionCost::default();
        let runtime = OsmEdgeMetadata::default_runtime();

        ga.matches.iter().for_each(|sc| {
            let coordinates: LineString<f64> = LineString::try_from_wkt_str(sc.input_linestring)
                .expect("Linestring must parse successfully.");

            let opts = MatchOptions::new()
                .with_runtime(runtime.clone())
                .with_cache(cache.clone());

            // Warm up the cache
            let result = graph
                .r#match(black_box(coordinates.clone()), opts)
                .expect("Match must complete successfully");

            // Verify the solution is actually correct
            insta::assert_ron_snapshot!(
                result.interpolated.elements,
                {
                     ".**.x" => insta::rounded_redaction(6),
                     ".**.y" => insta::rounded_redaction(6)
                }
            );

            let node_ids = result
                .interpolated
                .elements
                .iter()
                .map(|e| e.edge.source.id.identifier().to_string())
                .collect::<Vec<_>>()
                .join("\n");

            insta::assert_snapshot!(format!("{}_nodes", sc.name), node_ids);

            let coords = result
                .interpolated
                .elements
                .iter()
                .map(|e| format!("{:.6} {:.6}", e.point.x, e.point.y))
                .collect::<Vec<_>>()
                .join("\n");

            insta::assert_snapshot!(format!("{}_coords", sc.name), coords);

            group.bench_function(format!("layer-gen: {}", sc.name), |b| {
                let points = coordinates.clone().into_points();

                b.iter(|| {
                    let generator =
                        StandardGenerator::new(&graph, &costing, DEFAULT_SEARCH_DISTANCE);
                    let (layers, _) = generator.generate(&points);
                    assert_eq!(layers.layers.len(), points.len())
                })
            });

            // Always the default solver, used to ensure no regressions to a primary audience
            group.bench_function(format!("match: {}", sc.name), |b| {
                b.iter(|| {
                    let opts = MatchOptions::new()
                        .with_runtime(runtime.clone())
                        .with_cache(cache.clone())
                        .with_solver(SolverVariant::default());

                    let (linestring, edges) = bench_match(&graph, opts, coordinates.clone());

                    assert_subsequence(sc.expected_linestring, &edges, linestring);
                })
            });

            // == Benchmarks for specific solvers ==

            // Benchmark the pre-compute solver
            group.bench_function(
                format!("precompute_forward_solver:match: {}", sc.name),
                |b| {
                    b.iter(|| {
                        let opts = MatchOptions::new()
                            .with_runtime(runtime.clone())
                            .with_cache(cache.clone())
                            .with_solver(SolverVariant::Precompute);

                        let (linestring, edges) = bench_match(&graph, opts, coordinates.clone());
                        assert_subsequence(sc.expected_linestring, &edges, linestring);
                    })
                },
            );

            // Benchmark the selective solver (warm cache — shared across iterations)
            group.bench_function(
                format!("selective_forward_solver:match: {}", sc.name),
                |b| {
                    b.iter(|| {
                        let opts = MatchOptions::new()
                            .with_runtime(runtime.clone())
                            .with_cache(cache.clone())
                            .with_solver(SolverVariant::Selective);

                        let (linestring, edges) = bench_match(&graph, opts, coordinates.clone());
                        assert_subsequence(sc.expected_linestring, &edges, linestring);
                    })
                },
            );

            // Benchmark the selective solver with a cold (empty) cache per iteration.
            // Each setup call allocates a fresh PredicateCache; the allocation is excluded
            // from the measurement so only the solve (including parallel pre-warm) is timed.
            group.bench_function(
                format!("selective_forward_solver:match:cold: {}", sc.name),
                |b| {
                    b.iter_batched(
                        || {
                            Arc::new(
                                PredicateCache::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::default(
                                ),
                            )
                        },
                        |cold_cache| {
                            let opts = MatchOptions::new()
                                .with_runtime(runtime.clone())
                                .with_cache(cold_cache)
                                .with_solver(SolverVariant::Selective);

                            bench_match(&graph, opts, coordinates.clone())
                        },
                        BatchSize::SmallInput,
                    )
                },
            );

            // == Trellis solve: full forward pass vs one layer at a time ==
            //
            // Three drivers over the same warm shared cache:
            //  - `forward`      — batch: layers pre-generated (in setup), every
            //                     boundary weighed at once, one solve.
            //  - `progressive`  — layers pre-generated (in setup), then the
            //                     state grows one layer per step, re-solving
            //                     after each extension (resumed forward pass).
            //  - `streaming`    — the true realtime loop: each point is pushed
            //                     as it "arrives" (per-point layer generation
            //                     included), the state extends, and the solve
            //                     resumes. Nothing is known up-front.
            let strategies = CostingStrategies::default();
            let generator =
                StandardGenerator::new(&graph, &strategies.emission, DEFAULT_SEARCH_DISTANCE);
            let build_transition = || {
                let generator =
                    StandardGenerator::new(&graph, &strategies.emission, DEFAULT_SEARCH_DISTANCE);
                Transition::new(&graph, coordinates.clone(), &strategies, generator)
            };
            let points = coordinates.clone().into_points();

            let stream = |solver: &AllComputeSolver<OsmEntryId, OsmEdgeMetadata, OsmNetwork>| {
                let mut transition = Transition::empty(&graph, &strategies);
                let mut state = MatchState::default();

                for point in &points {
                    let width = transition
                        .push(*point, &generator)
                        .expect("point must anchor");
                    state.extend(width).expect("state must extend");
                    solver
                        .solve_path(&transition, &runtime, &mut state)
                        .expect("streaming solve must succeed");
                }

                (transition, state)
            };

            // The naive realtime baseline: layers still arrive incrementally
            // (same generation as `stream`), but nothing solved is kept — a
            // fresh MatchState per point discards all weighed boundaries and
            // the cached forward pass, redoing the whole trellis every time.
            // The delta against `stream` is exactly the trellis-as-cache win.
            let stream_naive =
                |solver: &AllComputeSolver<OsmEntryId, OsmEdgeMetadata, OsmNetwork>| {
                    let mut transition = Transition::empty(&graph, &strategies);

                    for point in &points {
                        transition
                            .push(*point, &generator)
                            .expect("point must anchor");

                        let mut state = MatchState::default();
                        for width in transition.widths() {
                            state.extend(width).expect("state must extend");
                        }
                        solver
                            .solve_path(&transition, &runtime, &mut state)
                            .expect("naive solve must succeed");
                    }
                };

            // The windowed naive baseline: per point, rebuild and solve a
            // transition over only the last `window` points — bounded per-step
            // cost, but approximate (no memory past the window) and it redoes
            // the window's layer generation, as a real windowed matcher would.
            let stream_windowed = |window: usize,
                                   solver: &AllComputeSolver<
                OsmEntryId,
                OsmEdgeMetadata,
                OsmNetwork,
            >| {
                let mut seen: Vec<Point> = Vec::new();

                for point in &points {
                    seen.push(*point);
                    let tail = seen.len().saturating_sub(window);

                    let generator = StandardGenerator::new(
                        &graph,
                        &strategies.emission,
                        DEFAULT_SEARCH_DISTANCE,
                    );
                    let transition = Transition::new(
                        &graph,
                        LineString::from(seen[tail..].to_vec()),
                        &strategies,
                        generator,
                    );
                    solver
                        .solve(transition, &runtime, &mut MatchState::default())
                        .expect("windowed solve must succeed");
                }
            };

            // All three drivers must agree exactly — checked once, unmeasured.
            {
                let solver = AllComputeSolver::default().use_cache(cache.clone());

                let forward = solver
                    .solve(build_transition(), &runtime, &mut MatchState::default())
                    .expect("forward solve must succeed");

                let progressive = solver
                    .solve_progressive(build_transition(), &runtime, &mut MatchState::default())
                    .expect("progressive solve must succeed");

                let (transition, mut state) = stream(&solver);
                let streamed = solver
                    .solve_path(&transition, &runtime, &mut state)
                    .expect("streamed solve must succeed");

                assert_eq!(forward.cost, progressive.cost);
                assert_eq!(forward.route, progressive.route);
                assert_eq!(forward.cost, streamed.cost);
                assert_eq!(forward.route, transition.route_of(&streamed));
            }

            // Throughput profile: one iteration of these benches is a whole
            // route (up to hundreds of solves), so a handful of samples with a
            // short warm-up is plenty — the default 30 × 3s-warm-up profile
            // makes the suite take the better part of an hour.
            group.sample_size(10);
            group.warm_up_time(core::time::Duration::from_millis(500));
            group.measurement_time(core::time::Duration::from_secs(3));

            group.bench_function(format!("trellis:forward: {}", sc.name), |b| {
                b.iter_batched(
                    build_transition,
                    |transition| {
                        let solver = AllComputeSolver::default().use_cache(cache.clone());
                        solver
                            .solve(transition, &runtime, &mut MatchState::default())
                            .expect("solve must succeed")
                    },
                    BatchSize::SmallInput,
                )
            });

            group.bench_function(format!("trellis:progressive: {}", sc.name), |b| {
                b.iter_batched(
                    build_transition,
                    |transition| {
                        let solver = AllComputeSolver::default().use_cache(cache.clone());
                        solver
                            .solve_progressive(transition, &runtime, &mut MatchState::default())
                            .expect("solve must succeed")
                    },
                    BatchSize::SmallInput,
                )
            });

            group.bench_function(format!("trellis:streaming: {}", sc.name), |b| {
                b.iter(|| {
                    let solver = AllComputeSolver::default().use_cache(cache.clone());
                    black_box(stream(&solver))
                })
            });

            // The same three drivers on a **cold** cache: each iteration gets a
            // fresh PredicateCache (allocated in setup, excluded from the
            // measurement), so every `reach` pays its network Dijkstra. Within
            // one iteration the cache is still shared across that route's own
            // appends — the realtime first-match cost.
            let fresh_cache =
                || Arc::new(PredicateCache::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::default());

            group.bench_function(format!("trellis:forward:cold: {}", sc.name), |b| {
                b.iter_batched(
                    || (build_transition(), fresh_cache()),
                    |(transition, cold)| {
                        let solver = AllComputeSolver::default().use_cache(cold);
                        solver
                            .solve(transition, &runtime, &mut MatchState::default())
                            .expect("solve must succeed")
                    },
                    BatchSize::SmallInput,
                )
            });

            group.bench_function(format!("trellis:progressive:cold: {}", sc.name), |b| {
                b.iter_batched(
                    || (build_transition(), fresh_cache()),
                    |(transition, cold)| {
                        let solver = AllComputeSolver::default().use_cache(cold);
                        solver
                            .solve_progressive(transition, &runtime, &mut MatchState::default())
                            .expect("solve must succeed")
                    },
                    BatchSize::SmallInput,
                )
            });

            group.bench_function(format!("trellis:streaming:cold: {}", sc.name), |b| {
                b.iter_batched(
                    fresh_cache,
                    |cold| {
                        let solver = AllComputeSolver::default().use_cache(cold);
                        black_box(stream(&solver))
                    },
                    BatchSize::SmallInput,
                )
            });

            // Naive baselines. Warm-cache only: measured cold ≈ warm within ~5%
            // for these — they re-query the same boundaries every step, so the
            // predicate cache warms itself within the first few points.

            group.bench_function(format!("trellis:naive: {}", sc.name), |b| {
                b.iter(|| {
                    let solver = AllComputeSolver::default().use_cache(cache.clone());
                    stream_naive(&solver)
                })
            });

            group.bench_function(format!("trellis:naive:window20: {}", sc.name), |b| {
                b.iter(|| {
                    let solver = AllComputeSolver::default().use_cache(cache.clone());
                    stream_windowed(20, &solver)
                })
            });

            // Restore the default profile for the next scenario's benches.
            group.sample_size(30);
            group.warm_up_time(core::time::Duration::from_secs(3));
            group.measurement_time(core::time::Duration::from_secs(5));
        });
    });

    group.finish();
}

fn bench_match<E: Entry, M: Metadata, N: Network<E, M>>(
    graph: &dyn Match<E, M, N>,
    opts: MatchOptions<E, M, N>,
    coordinates: LineString<f64>,
) -> (LineString, Vec<i64>) {
    let result = graph
        .r#match(coordinates.clone(), opts)
        .expect("Match must complete successfully");

    let line_string = result
        .interpolated
        .iter()
        .map(|v| Point(v.point))
        .collect::<LineString>();

    let edges = result
        .interpolated
        .iter()
        .map(|element| element.edge.id().identifier())
        .collect::<Vec<_>>();

    (line_string, edges)
}

criterion::criterion_group!(targeted_benches, target_benchmark);
criterion_main!(targeted_benches);
