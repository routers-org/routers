use routers_fixtures::{
    LAX_LYNWOOD_MATCHED, LAX_LYNWOOD_TRIP, LOS_ANGELES, VENTURA_MATCHED, VENTURA_TRIP, ZURICH,
    fixture,
};

use routers::transition::*;
use routers::{Graph, Match};

use routers_codec::osm::OsmEdgeMetadata;
use routers_codec::{Entry, Metadata};

use criterion::{black_box, criterion_main};
use geo::LineString;
use std::path::Path;
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

fn assert_subsequence(a: &[i64], b: &[i64]) {
    let mut a_iter = a.iter();

    for b_item in b {
        if !a_iter.any(|a_item| a_item == b_item) {
            panic!(
                "b is not a subsequence of a: element {b_item} not found in remaining portion of a",
            );
        }
    }
}

fn target_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group("match");

    group.significance_level(0.1).sample_size(30);

    MATCH_CASES.into_iter().for_each(|ga| {
        let path = Path::new(fixture!(ga.source_file))
            .as_os_str()
            .to_ascii_lowercase();
        let graph = Graph::new(path).expect("Graph must be created");

        let costing = CostingStrategies::default();
        let runtime = OsmEdgeMetadata::runtime(None);

        ga.matches.iter().for_each(|sc| {
            let coordinates: LineString<f64> = LineString::try_from_wkt_str(sc.input_linestring)
                .expect("Linestring must parse successfully.");

            // Warm up the cache
            let _ = graph
                .r#match(
                    &runtime,
                    SolverVariant::Fast,
                    black_box(coordinates.clone()),
                )
                .expect("Match must complete successfully");

            group.bench_function(format!("layer-gen: {}", sc.name), |b| {
                let points = coordinates.clone().into_points();
                let generator = LayerGenerator::new(&graph, &costing);

                b.iter(|| {
                    let (layers, _) = generator.with_points(&points);
                    assert_eq!(layers.layers.len(), points.len())
                })
            });

            // Always the default solver, used to ensure no regressions to a primary audiance
            group.bench_function(format!("match: {}", sc.name), |b| {
                b.iter(|| {
                    let edges =
                        bench_match(&graph, &runtime, coordinates.clone(), SolverVariant::Fast);

                    assert_subsequence(sc.expected_linestring, &edges);
                })
            });

            // == Benchmarks for specific solvers ==

            // Benchmark the fast layer sweep solver
            group.bench_function(format!("fast_layer_sweep_solver:match: {}", sc.name), |b| {
                b.iter(|| {
                    let edges =
                        bench_match(&graph, &runtime, coordinates.clone(), SolverVariant::Fast);

                    assert_subsequence(sc.expected_linestring, &edges);
                })
            });

            // Benchmark the pre-compute solver
            // group.bench_function(
            //     format!("precompute_forward_solver:match: {}", sc.name),
            //     |b| {
            //         b.iter(|| {
            //             let edges = bench_match(
            //                 &graph,
            //                 &runtime,
            //                 coordinates.clone(),
            //                 SolverVariant::Precompute,
            //             );
            //
            //             assert_subsequence(sc.expected_linestring, &edges);
            //         })
            //     },
            // );
            //
            // // Benchmark the selective solver
            // group.bench_function(
            //     format!("selective_forward_solver:match: {}", sc.name),
            //     |b| {
            //         b.iter(|| {
            //             let edges = bench_match(
            //                 &graph,
            //                 &runtime,
            //                 coordinates.clone(),
            //                 SolverVariant::Selective,
            //             );
            //
            //             assert_subsequence(sc.expected_linestring, &edges);
            //         })
            //     },
            // );
        });
    });

    group.measurement_time(std::time::Duration::from_secs(20));
    group.sample_size(100);

    group.finish();
}

fn bench_match<E: Entry, M: Metadata>(
    graph: &Graph<E, M>,
    runtime: &M::Runtime,
    coordinates: LineString<f64>,
    solver: impl Into<SolverVariant>,
) -> Vec<i64> {
    let result = graph
        .r#match(runtime, solver, coordinates.clone())
        .expect("Match must complete successfully");

    result
        .interpolated
        .iter()
        .map(|element| element.edge.id().identifier())
        .collect::<Vec<_>>()
}

criterion::criterion_group!(targeted_benches, target_benchmark);
criterion_main!(targeted_benches);
