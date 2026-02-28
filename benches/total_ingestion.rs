use routers_codec::osm::OsmNetwork;
use routers_fixtures::{DISTRICT_OF_COLUMBIA, fixture_path};

use criterion::criterion_main;
use log::info;

use std::path::Path;

fn ingest_as_full_graph() {
    let path = Path::new(fixture_path(DISTRICT_OF_COLUMBIA).as_os_str())
        .as_os_str()
        .to_ascii_lowercase();
    let graph = OsmNetwork::new(path).expect("Could not generate graph");
    info!("OSM network generated with {} nodes", graph.num_nodes());
}

fn ingestion_benchmark(c: &mut criterion::Criterion) {
    let mut group = c.benchmark_group("ingestion_benchmark");
    group.significance_level(0.1).sample_size(30);

    group.bench_function("ingest_as_full_graph", |b| b.iter(ingest_as_full_graph));
    group.finish();
}

criterion::criterion_group!(standard_benches, ingestion_benchmark);
criterion_main!(standard_benches);
