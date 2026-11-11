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

/// A lookup plus the offset and DST flag at an instant, as the services do.
fn resolve_offset() {
    let rect = point! { x: 151.208211, y: -33.871075 }.bounding_rect();
    let at = routers_tz::Timestamp::from_second(1_784_073_600).expect("in range");

    let resolver = RESOLVER.get().expect("timezones not initialized");
    for tz in resolver.search(&rect).expect("should have been resolved") {
        let info = tz.at(at);
        black_box((info.offset(), info.dst()));
    }
}

/// `resolve_offset` from several threads at once; each thread's time per lookup.
fn resolve_offset_parallel(iters: u64) -> std::time::Duration {
    const THREADS: usize = 8;

    let start = std::time::Instant::now();
    std::thread::scope(|scope| {
        for _ in 0..THREADS {
            scope.spawn(|| (0..iters).for_each(|_| resolve_offset()));
        }
    });
    start.elapsed()
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
    group.bench_function("resolve_offset", |b| b.iter(resolve_offset));
    group.bench_function("resolve_offset_parallel", |b| {
        b.iter_custom(resolve_offset_parallel)
    });
    group.finish();
}

criterion::criterion_group!(standard_benches, discovery_benchmark);
criterion_main!(standard_benches);
