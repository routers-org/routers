//! [`Partition`] must yield exactly the shards `from_source` would build
//! for every owning cell under `OwnedAndPadded`, in ascending id order.

mod common;

use alloc::collections::{BTreeMap, BTreeSet};
use common::MemSource;
use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_shard::{
    GeohashStrategy, Selection, SelectionMode, ShardSource, ShardedNetwork, ShardingStrategy,
};

extern crate alloc;

type Net<S> = ShardedNetwork<OsmEntryId, OsmEdgeMetadata, S>;
type Fingerprint = (
    BTreeSet<OsmEntryId>,
    BTreeSet<(OsmEntryId, OsmEntryId, u32)>,
    BTreeMap<OsmEntryId, Option<core::num::NonZeroU8>>,
);

/// Order-independent view of a shard's contents.
fn fingerprint<S: routers_shard::ShardId>(net: &Net<S>) -> Fingerprint {
    let nodes = net.hash.keys().copied().collect();
    let edges = net
        .graph
        .all_edges()
        .map(|(s, t, (w, _))| (s, t, *w))
        .collect();
    // Which edge's metadata a node kept, not just whether it kept one.
    let meta = net.meta.iter().map(|(k, m)| (*k, m.lane_count)).collect();
    (nodes, edges, meta)
}

fn reference<S: routers_shard::ShardId>(
    source: &MemSource,
    strategy: &impl ShardingStrategy<Id = S>,
    padding: f64,
) -> BTreeMap<S, Net<S>> {
    let cells: BTreeSet<S> = source.nodes().map(|(_, p)| strategy.locate(p)).collect();
    cells
        .into_iter()
        .map(|cell| {
            let selection = Selection::new(
                strategy,
                cell,
                SelectionMode::OwnedAndPadded {
                    padding_distance: padding,
                },
            );
            let net = ShardedNetwork::from_source(source, strategy, &selection).expect("build");
            (cell, net)
        })
        .collect()
}

fn assert_equivalent(source: &MemSource, precision: u8, padding: f64) {
    let strategy = GeohashStrategy::with_precision(precision);
    let expected = reference(source, &strategy, padding);

    let partition =
        ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, _>::partition(source, &strategy, padding);
    assert_eq!(partition.len(), expected.len(), "shard count");

    let mut seen = Vec::new();
    for net in partition {
        let reference = expected
            .get(&net.owned)
            .unwrap_or_else(|| panic!("unexpected shard {}", net.owned));
        assert_eq!(net.loaded, reference.loaded, "{}: loaded", net.owned);
        assert_eq!(
            fingerprint(&net),
            fingerprint(reference),
            "{}: contents differ",
            net.owned
        );
        assert_eq!(
            net.index.len(),
            reference.index.len(),
            "{}: node index",
            net.owned
        );
        assert_eq!(
            net.index_edge.len(),
            reference.index_edge.len(),
            "{}: edge index",
            net.owned
        );
        seen.push(net.owned);
    }

    assert!(seen.windows(2).all(|w| w[0] < w[1]), "ascending id order");
    assert_eq!(seen.len(), expected.len());
}

#[test]
fn matches_from_source_at_fine_precision() {
    // ~11 m grid pitch; precision-7 cells are ~150 m, so a 50 m buffer is
    // the classic owner-plus-eight-neighbours case.
    let source = MemSource::grid(Point::new(13.4, 52.5), 60, 60, 0.0001);
    assert_equivalent(&source, 7, 50.0);
}

#[test]
fn matches_from_source_when_padding_exceeds_cell_size() {
    // Precision-8 cells are ~38 m × 19 m; a 200 m buffer reaches several
    // rings out, so the neighbourhood walk must go past the first ring.
    let source = MemSource::grid(Point::new(13.4, 52.5), 30, 30, 0.0001);
    assert_equivalent(&source, 8, 200.0);
}

#[test]
fn matches_from_source_at_coarse_precision() {
    // A thin strip relative to ~150 km cells.
    let source = MemSource::grid(Point::new(13.0, 52.0), 80, 80, 0.01);
    assert_equivalent(&source, 3, 1000.0);
}

#[test]
fn matches_from_source_with_zero_padding() {
    let source = MemSource::grid(Point::new(13.4, 52.5), 40, 40, 0.0001);
    assert_equivalent(&source, 7, 0.0);
}

#[test]
fn retain_keeps_only_matching_shards_in_order() {
    let source = MemSource::grid(Point::new(13.4, 52.5), 60, 60, 0.0001);
    let strategy = GeohashStrategy::with_precision(7);
    let all: Vec<_> =
        ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, _>::partition(&source, &strategy, 50.0)
            .map(|net| net.owned)
            .collect();
    let prefix = all[0].to_string()[..6].to_owned();
    let expected: Vec<_> = all
        .iter()
        .copied()
        .filter(|id| id.to_string().starts_with(&prefix))
        .collect();
    assert!(!expected.is_empty() && expected.len() < all.len());

    let mut partition =
        ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, _>::partition(&source, &strategy, 50.0);
    partition.retain(|id| id.to_string().starts_with(&prefix));
    assert_eq!(partition.len(), expected.len());
    let kept: Vec<_> = partition.map(|net| net.owned).collect();
    assert_eq!(kept, expected);
}

#[test]
fn without_indices_yields_identical_content_and_empty_indices() {
    let source = MemSource::grid(Point::new(13.4, 52.5), 40, 40, 0.0001);
    let strategy = GeohashStrategy::with_precision(7);
    let indexed: Vec<_> =
        ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, _>::partition(&source, &strategy, 50.0)
            .collect();
    let bare: Vec<_> =
        ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, _>::partition(&source, &strategy, 50.0)
            .without_indices()
            .collect();
    assert_eq!(indexed.len(), bare.len());
    for (a, b) in indexed.iter().zip(&bare) {
        assert_eq!(a.owned, b.owned);
        assert_eq!(fingerprint(a), fingerprint(b));
        assert!(a.index.len() > 0);
        assert_eq!(b.index.len(), 0);
        assert_eq!(b.index_edge.len(), 0);
    }
    // A bare shard round-trips through the cache format to a fully indexed one.
    let bytes = bare[0].to_cache_bytes().expect("encode");
    let loaded: ShardedNetwork<OsmEntryId, OsmEdgeMetadata, routers_shard::Geohash> =
        ShardedNetwork::from_cached_bytes(&bytes).expect("decode");
    assert_eq!(loaded.index.len(), indexed[0].index.len());
}
