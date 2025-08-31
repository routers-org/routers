use routers_fixtures::{DISTRICT_OF_COLUMBIA, fixture_path};

use routers_codec::osm::{
    BlockItem, BlockIterator, Parallel, ProcessedElementIterator, element::ProcessedElement,
};

use criterion::criterion_main;
use log::{error, info};
use rayon::iter::ParallelIterator;
use std::time::Instant;

fn iterate_blocks_each() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let iterator = BlockIterator::new(path.clone());

    let mut primitive_blocks = 0;
    let mut header_blocks = 0;

    match iterator {
        Ok(iter) => {
            for block in iter {
                match block {
                    BlockItem::HeaderBlock(_) => header_blocks += 1,
                    BlockItem::PrimitiveBlock(_) => primitive_blocks += 1,
                }
            }
        }
        Err(err) => {
            error!(
                "Failed to load file, {:?}. Got error: {err}",
                path.as_os_str().to_str()
            );
        }
    }

    assert_eq!(header_blocks, 1);
    assert_eq!(primitive_blocks, 21);
}

fn parallel_iterate_blocks_each() {
    let path = fixture_path(DISTRICT_OF_COLUMBIA);

    let mut block_iter = BlockIterator::new(path).unwrap();

    let elements = block_iter
        .par_iter()
        .map(|block| match block {
            BlockItem::HeaderBlock(_) => (0, 1),
            BlockItem::PrimitiveBlock(_) => (1, 0),
        })
        .reduce(|| (0, 0), |a, b| (a.0 + b.0, a.1 + b.1));

    assert_eq!(elements, (21, 1));
}

fn compare_to_osmpbf() {
    use osmpbf::{BlobReader, BlobType};

    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let reader = BlobReader::from_path(path).unwrap();

    let mut primitive_blocks = 0;
    let mut header_blocks = 0;

    for blob in reader.flatten() {
        match blob.get_type() {
            BlobType::OsmHeader => {
                if blob.to_headerblock().is_ok() {
                    header_blocks += 1;
                }
            }
            BlobType::OsmData => {
                if blob.to_primitiveblock().is_ok() {
                    primitive_blocks += 1;
                }
            }
            _ => {}
        }
    }

    assert_eq!(header_blocks, 1);
    assert_eq!(primitive_blocks, 21);
}

fn ingest_and_count() {
    let timer = Instant::now();
    let path = fixture_path(DISTRICT_OF_COLUMBIA);
    let reader = ProcessedElementIterator::new(path).expect("!");

    let (ways, nodes) = reader.par_red(
        |(ways, nodes), element| match element {
            ProcessedElement::Way(_) => (ways + 1, nodes),
            ProcessedElement::Node(_) => (ways, nodes + 1),
            _ => (ways, nodes),
        },
        |(ways, nodes), (ways2, nodes2)| (ways + ways2, nodes + nodes2),
        || (0u64, 0u64),
    );

    info!(
        "Got {} ways and {} nodes in {}ms",
        ways,
        nodes,
        timer.elapsed().as_millis()
    );
}

fn target_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group("iterator_target");
    group.significance_level(0.1).sample_size(30);

    group.bench_function("iterate_blocks_each", |b| b.iter(iterate_blocks_each));
    group.bench_function("parallel_iterate_blocks_each", |b| {
        b.iter(parallel_iterate_blocks_each)
    });
    group.bench_function("compared_to_osmpbf", |b| b.iter(compare_to_osmpbf));
    group.bench_function("ingest_and_count", |b| b.iter(ingest_and_count));

    group.finish();
}

criterion::criterion_group!(targeted_benches, target_benchmark);
criterion_main!(targeted_benches);
