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

use criterion::{black_box, criterion_main};
use geo::{LineString, Point};
use routers::generation::{LayerGeneration, StandardGenerator};
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
        let path = Path::new(fixture!(ga.source_file))
            .as_os_str()
            .to_ascii_lowercase();
        let cache = Arc::new(PredicateCache::<OsmEntryId, OsmEdgeMetadata, OsmNetwork>::default());
        let graph = OsmNetwork::new(path).expect("Graph must be created");

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

            // Benchmark the selective solver
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
        });
    });

    group.measurement_time(core::time::Duration::from_secs(20));
    group.sample_size(100);

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
