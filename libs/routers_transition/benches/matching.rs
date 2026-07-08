//! Regression benchmarks for the map-matching pipeline.
//!
//! These are **not** A/B races between solvers — each solver × size is its own
//! tracked line so `criterion` can tell us whether a change regressed (or
//! improved) that specific workload over time. Inputs are deterministic
//! synthetic `MockNetwork`s so results are comparable across runs.
//!
//! Run: `cargo bench -p routers_transition`
//! Compare against a saved baseline with criterion's `--save-baseline` / `--baseline`.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use geo::{Coord, LineString, point};
use routers_network::mock::{MockNetwork, MockNetworkBuilder};
use routers_transition::Transition;
use routers_transition::r#match::MatchSimpleExt;

/// A straight west-bound road of `n` nodes (`n-1` unit edges), ~92 m spacing.
fn straight_net(n: usize) -> MockNetwork {
    let mut b = MockNetworkBuilder::new();
    for i in 0..n {
        let x = -118.15 - (i as f64) * 0.001;
        b = b.node(i as i64 + 1, point!(x: x, y: 34.15));
    }
    for i in 0..n.saturating_sub(1) {
        b = b.edge(i as i64 + 1, i as i64 + 2);
    }
    b.build()
}

/// A trajectory of `points` positions drifted ~33 m north of the road.
fn trip(points: usize) -> LineString {
    LineString::new(
        (0..points)
            .map(|i| Coord {
                x: -118.151 - (i as f64) * 0.001,
                y: 34.1503,
            })
            .collect(),
    )
}

/// Full match (layer generation + weigh + graph solve + reconstruction) across
/// route lengths. The single most important regression signal for the pipeline.
fn bench_full_match(c: &mut Criterion) {
    let mut group = c.benchmark_group("full_match");
    for &len in &[8usize, 32, 128] {
        let net = straight_net(len + 4);
        let ls = trip(len);
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |bench, _| {
            bench.iter(|| net.match_simple(ls.clone()).expect("match must succeed"));
        });
    }
    group.finish();
}

/// Layer generation in isolation (`Transition::new` runs the `StandardGenerator`:
/// candidate projection + emission costing). Isolates the non-solve half so a
/// regression there is distinguishable from a solver regression.
fn bench_layer_generation(c: &mut Criterion) {
    use routers_transition::generation::StandardGenerator;
    use routers_transition::{CostingStrategies, DEFAULT_SEARCH_DISTANCE};

    let mut group = c.benchmark_group("layer_generation");
    for &len in &[8usize, 32, 128] {
        let net = straight_net(len + 4);
        let ls = trip(len);
        let costing = CostingStrategies::default();
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |bench, _| {
            bench.iter(|| {
                let generator =
                    StandardGenerator::new(&net, &costing.emission, DEFAULT_SEARCH_DISTANCE);
                Transition::new(&net, ls.clone(), &costing, generator)
            });
        });
    }
    group.finish();
}

criterion_group!(benches, bench_full_match, bench_layer_generation);
criterion_main!(benches);
