use routers_fixtures::{
    LAX_LYNWOOD_MATCHED, LAX_LYNWOOD_TRIP, LOS_ANGELES, VENTURA_MATCHED, VENTURA_TRIP, ZURICH,
    fixture,
};

use routers::transition::*;
use routers::{Graph, Match};

use codec::Metadata;
use codec::osm::OsmEdgeMetadata;
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
                "b is not a subsequence of a: element {} not found in remaining portion of a",
                b_item
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
                    PrecomputeForwardSolver::default().use_cache(graph.cache.clone()),
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

            group.bench_function(format!("match: {}", sc.name), |b| {
                b.iter(|| {
                    // Yield the transition layers of each level
                    // & Collapse the layers into a final vector
                    let solver = PrecomputeForwardSolver::default();

                    let result = graph
                        .r#match(
                            &runtime,
                            solver.use_cache(graph.cache.clone()),
                            coordinates.clone(),
                        )
                        .expect("Match must complete successfully");

                    let edges = result
                        .interpolated
                        .iter()
                        .map(|element| element.edge.id().identifier)
                        .collect::<Vec<_>>();

                    assert_subsequence(sc.expected_linestring, &edges);
                })
            });
        });
    });

    group.finish();
}

criterion::criterion_group!(targeted_benches, target_benchmark);
criterion_main!(targeted_benches);
