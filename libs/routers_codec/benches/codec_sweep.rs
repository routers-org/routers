use routers_fixtures::{DISTRICT_OF_COLUMBIA, fixture_path};

use routers_codec::osm::{
    BlockIterator, Element, ElementIterator, Parallel, ProcessedElementIterator,
    element::ProcessedElement,
};

use core::any::Any;
use criterion::criterion_main;
use log::info;
use rayon::iter::ParallelIterator;

fn block_iter_count() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let mut iter = BlockIterator::new(path).expect("Could not create iterator");

    iter.par_iter().for_each(|item| {
        info!("Block: {:?}", item.type_id());
    });
}

fn element_iter_count() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let iter = ElementIterator::new(path).expect("Could not create iterator");

    let nodes = iter.map_red(
        |item| match item {
            Element::Way(_) => 0,
            Element::Node(_) => 1,
            Element::Relation(_) => 0,
            Element::DenseNodes(_) => 0,
        },
        |a, b| a + b,
        || 0,
    );

    info!("There are {nodes} nodes");
}

fn processed_iter_count() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let iter = ProcessedElementIterator::new(path).expect("Could not create iterator");

    let nodes = iter.map_red(
        |item| match item {
            ProcessedElement::Node(_) => 1,
            _ => 0,
        },
        |a, b| a + b,
        || 0,
    );

    info!("There are {nodes} nodes");
}

fn sweep_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group("iterator_sweep");
    group.significance_level(0.1).sample_size(30);

    group.bench_function("block_iter_count", |b| b.iter(block_iter_count));
    group.bench_function("element_iter_count", |b| b.iter(element_iter_count));
    group.bench_function("processed_iter_count", |b| b.iter(processed_iter_count));
    group.finish();
}

criterion::criterion_group!(standard_benches, sweep_benchmark);
criterion_main!(standard_benches);
