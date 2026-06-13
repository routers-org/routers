//! Per-K performance sweep for the streaming matcher.
//!
//! For each `MATCH_FRONTIER_K` value, this bench cold-starts on a
//! 2-point prefix of a real trip, then warm-steps through the
//! remaining points while applying `truncate_to_top(k)` between
//! events. The bench reports the cost of an entire trip's
//! streaming chain — divide by `points.len() - 2` for per-step.

use criterion::{Criterion, black_box, criterion_main};
use geo::{LineString, Point};
use routers::transition::r#match::DEFAULT_SEARCH_DISTANCE;
use routers::transition::streaming::StreamingMatcher;
use routers::transition::CostingStrategies;
use routers_codec::osm::{OsmEdgeMetadata, OsmNetwork};
use routers_fixtures::{LOS_ANGELES, VENTURA_TRIP, fixture};
use routers_network::traits::metadata::Metadata;
use wkt::TryFromWkt;

fn streaming_match_bench(c: &mut Criterion) {
    let graph = OsmNetwork::from_pbf(fixture!(LOS_ANGELES)).expect("graph loads");
    let ls: LineString<f64> =
        LineString::try_from_wkt_str(VENTURA_TRIP).expect("trip linestring parses");
    let points: Vec<Point> = ls.into_points();
    assert!(points.len() >= 3, "trip must have at least 3 points");

    let costing = CostingStrategies::default();
    let matcher = StreamingMatcher::new(&graph, &costing, DEFAULT_SEARCH_DISTANCE);
    let runtime = OsmEdgeMetadata::default_runtime();

    let mut group = c.benchmark_group("streaming_match");
    group.sample_size(30);
    group.significance_level(0.1);

    for k in [1usize, 4, 8, 16, usize::MAX] {
        let label = if k == usize::MAX {
            "inf".to_string()
        } else {
            k.to_string()
        };
        group.bench_function(format!("warm_chain_k{label}"), |b| {
            b.iter(|| {
                let seed = LineString(vec![points[0].into(), points[1].into()]);
                let (_, mut state) = matcher
                    .cold_start(seed, 0, &runtime)
                    .expect("cold-start solves");
                state.truncate_to_top(k);

                for (i, p) in points.iter().enumerate().skip(2) {
                    let (_, mut new_state) = matcher
                        .step(&state, *p, i as u64, &runtime)
                        .expect("warm step solves");
                    new_state.truncate_to_top(k);
                    state = new_state;
                }
                black_box(state);
            });
        });
    }

    group.finish();
}

criterion::criterion_group!(streaming_benches, streaming_match_bench);
criterion_main!(streaming_benches);
