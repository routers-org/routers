use criterion::{black_box, criterion_main};
use geo::{BoundingRect, Rect, point};
use routers_tz::TimezoneResolver;
use std::sync::OnceLock;

#[cfg(feature = "rtree")]
pub static RESOLVER: OnceLock<routers_tz::RTreeStorage> = OnceLock::new();

#[cfg(feature = "s2cell")]
pub static RESOLVER: OnceLock<routers_tz::S2CellStorage> = OnceLock::new();

#[cfg(feature = "basic")]
pub static RESOLVER: OnceLock<routers_tz::BasicStorage> = OnceLock::new();

fn init() {
    #[cfg(feature = "rtree")]
    RESOLVER.get_or_init(|| routers_tz::RTreeStorage::default());

    #[cfg(feature = "basic")]
    RESOLVER.get_or_init(|| routers_tz::BasicStorage::default());

    #[cfg(feature = "s2cell")]
    RESOLVER.get_or_init(|| routers_tz::S2CellStorage::default());
}

pub fn run_singular(rect: &Rect) {
    let possible_timezones = RESOLVER
        .get()
        .expect("timezones not initialized")
        .search(rect)
        .expect("should have been resolved");

    black_box(possible_timezones);
}

fn search_sparse() {
    let rect = point! { x: 151.208211, y: -33.871075 }.bounding_rect();

    run_singular(&rect);
}

#[cfg(feature = "rtree")]
const BACKEND: &str = "rtree";
#[cfg(feature = "s2cell")]
const BACKEND: &str = "s2cell";
#[cfg(feature = "basic")]
const BACKEND: &str = "basic";

fn discovery_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group(BACKEND);
    group.significance_level(0.1).sample_size(30);

    init();

    group.bench_function("search_sparse", |b| b.iter(search_sparse));
    group.finish();
}

criterion::criterion_group!(standard_benches, discovery_benchmark);
criterion_main!(standard_benches);
