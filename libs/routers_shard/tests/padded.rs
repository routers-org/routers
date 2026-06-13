//! End-to-end tests for [`SelectionMode::OwnedAndPadded`].
//!
//! Verifies that the padded buffer:
//!
//! - Pulls in raw graph nodes lying just outside the owned shard, even
//!   when those nodes' own shards are not loaded.
//! - Leaves the `loaded` set as `{owned}`, so no whole neighbour shard
//!   is materialised regardless of precision.
//! - Excludes nodes that fall outside both the owned shard and the
//!   buffer.

mod common;

use common::MemSource;
use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_shard::{GeohashStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy};

fn build<S: routers_shard::ShardId>(
    source: &MemSource,
    strategy: &impl ShardingStrategy<Id = S>,
    selection: &Selection<S>,
) -> ShardedNetwork<OsmEntryId, OsmEdgeMetadata, S> {
    ShardedNetwork::from_source(source, strategy, selection).expect("build")
}

#[test]
fn padded_buffer_admits_nodes_outside_owned_shard() {
    // Dense grid at sub-metre pitch so the buffer admits a measurable
    // number of nodes outside the owned cell.
    let origin = Point::new(13.4, 52.5);
    let step = 0.0001; // ~11 m N/S
    let source = MemSource::grid(origin, 40, 40, step);
    let strategy = GeohashStrategy::with_precision(7);
    let owned = strategy.locate(Point::new(
        origin.x() + 20.0 * step,
        origin.y() + 20.0 * step,
    ));

    let owned_only = Selection::new(&strategy, owned, SelectionMode::Owned);
    let padded = Selection::new(
        &strategy,
        owned,
        SelectionMode::OwnedAndPadded {
            padding_distance: 50.0,
        },
    );

    let net_owned = build(&source, &strategy, &owned_only);
    let net_padded = build(&source, &strategy, &padded);

    assert!(
        net_padded.num_nodes() > net_owned.num_nodes(),
        "padded buffer should admit extra nodes (owned={}, padded={})",
        net_owned.num_nodes(),
        net_padded.num_nodes(),
    );
}

#[test]
fn padded_mode_does_not_load_extra_shards() {
    // Even when the buffer admits cross-boundary nodes, the resulting
    // network's `loaded` set must remain just the owned shard — the
    // whole point of the mode at coarse precision.
    let source = MemSource::grid(Point::new(13.4, 52.5), 40, 40, 0.0001);
    let strategy = GeohashStrategy::with_precision(7);
    let owned = strategy.locate(Point::new(13.402, 52.502));

    let padded = Selection::new(
        &strategy,
        owned,
        SelectionMode::OwnedAndPadded {
            padding_distance: 50.0,
        },
    );

    let net = build(&source, &strategy, &padded);
    assert_eq!(net.loaded.len(), 1);
    assert!(net.loaded.contains(&owned));
}

#[test]
fn larger_buffer_admits_more_nodes_than_smaller_buffer() {
    // Monotonicity: widening the buffer can only ever admit more
    // primary nodes, never fewer.
    let source = MemSource::grid(Point::new(13.4, 52.5), 80, 80, 0.0001);
    let strategy = GeohashStrategy::with_precision(7);
    let owned = strategy.locate(Point::new(13.404, 52.504));

    let small = build(
        &source,
        &strategy,
        &Selection::new(
            &strategy,
            owned,
            SelectionMode::OwnedAndPadded {
                padding_distance: 10.0,
            },
        ),
    );
    let large = build(
        &source,
        &strategy,
        &Selection::new(
            &strategy,
            owned,
            SelectionMode::OwnedAndPadded {
                padding_distance: 100.0,
            },
        ),
    );

    assert!(
        large.num_nodes() >= small.num_nodes(),
        "larger buffer should admit at least as many nodes (small={}, large={})",
        small.num_nodes(),
        large.num_nodes(),
    );
    assert!(
        large.num_nodes() > small.num_nodes(),
        "with a meaningfully larger buffer we expect strictly more nodes",
    );
}

#[test]
fn padded_at_coarse_precision_admits_strip_without_whole_neighbours() {
    // Geohash precision 3 cells are ~150 km wide; a 200 m buffer at
    // those scales is a thin strip. The padded network should be
    // strictly smaller than the OwnedAndNeighbours equivalent (which
    // would pull in the surrounding 150-km cells in full) while still
    // admitting some cross-boundary nodes vs. Owned alone.
    let source = MemSource::grid(Point::new(13.0, 52.0), 200, 200, 0.01);
    let strategy = GeohashStrategy::with_precision(3);
    let owned = strategy.locate(Point::new(13.5, 52.5));

    let owned_only = build(
        &source,
        &strategy,
        &Selection::new(&strategy, owned, SelectionMode::Owned),
    );
    let padded = build(
        &source,
        &strategy,
        &Selection::new(
            &strategy,
            owned,
            SelectionMode::OwnedAndPadded {
                padding_distance: 200.0,
            },
        ),
    );
    let full = build(
        &source,
        &strategy,
        &Selection::new(&strategy, owned, SelectionMode::OwnedAndNeighbours),
    );

    assert!(padded.num_nodes() >= owned_only.num_nodes());
    assert!(
        padded.num_nodes() < full.num_nodes(),
        "padded ({}) should be much smaller than full neighbours ({}) at coarse precision",
        padded.num_nodes(),
        full.num_nodes(),
    );
}
