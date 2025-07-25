use criterion::{black_box, criterion_main};
use geo::{BoundingRect, Point, point};
use routers_tz::{RTreeStorage, TimezoneResolver};
use routers_tz_types::timezone::ResolvedTimezones;
use std::path::Path;
use std::sync::OnceLock;
use std::time::Instant;

pub static RESOLVER: OnceLock<RTreeStorage> = OnceLock::new();

fn init() {
    RESOLVER.get_or_init(|| {
        use crate::RTreeStorage;
        return RTreeStorage::new();
    });
}

pub fn assert_singular(point: Point, expected: &str) {
    let possible_timezones = RESOLVER
        .get()
        .expect("timezones not initialized")
        .search(&point.bounding_rect())
        .expect("should have been resolved");

    black_box(possible_timezones);
}

fn search_sparse() {
    assert_singular(point! { x: 151.208211, y: -33.871075 }, "Australia/Sydney");
}

fn discovery_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group("discovery_benchmark");
    group.significance_level(0.1).sample_size(30);

    init();

    group.bench_function("search_sparse", |b| b.iter(search_sparse));
    group.finish();
}

criterion::criterion_group!(standard_benches, discovery_benchmark);
criterion_main!(standard_benches);
