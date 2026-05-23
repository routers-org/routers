#![cfg(feature = "osm")]

use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_fixtures::{SYDNEY, fixture_path};
use routers_network::DataPlane;
use routers_shard::{
    QuadKey, QuadTreeStrategy, Selection, SelectionMode, ShardedNetwork, ShardingStrategy,
    osm::OsmSource,
};

fn build(
    mode: SelectionMode,
) -> (
    ShardedNetwork<OsmEntryId, OsmEdgeMetadata, QuadKey>,
    QuadKey,
) {
    let path = fixture_path(SYDNEY);
    let source = OsmSource::new(path);
    let strategy = QuadTreeStrategy::with_depth(10);

    // Anchor on a point in central Sydney.
    let owned = strategy.locate(Point::new(151.2093, -33.8688));
    let selection = Selection::new(&strategy, owned, mode);

    let net =
        ShardedNetwork::from_source(&source, &strategy, &selection).expect("ingest must succeed");
    (net, owned)
}

#[test]
fn owned_only_builds_nonempty_network() {
    let (net, _) = build(SelectionMode::Owned);
    assert!(
        net.num_nodes() > 0,
        "owned shard should contain some road network"
    );
    assert!(net.graph.edge_count() > 0);
}

#[test]
fn neighbours_grow_the_loaded_set() {
    let (net_owned, _) = build(SelectionMode::Owned);
    let (net_with_neighbours, _) = build(SelectionMode::OwnedAndNeighbours);

    assert!(
        net_with_neighbours.loaded.len() > net_owned.loaded.len(),
        "neighbour selection must load more shards"
    );
    assert!(
        net_with_neighbours.num_nodes() >= net_owned.num_nodes(),
        "padding cannot remove nodes"
    );
}

#[test]
fn ingested_nodes_are_in_loaded_shards_or_their_halo() {
    // Every retained node should either fall inside a loaded shard or be a
    // one-hop reference from a way that touches the selection.
    let (net, _) = build(SelectionMode::Owned);
    let strategy = QuadTreeStrategy::with_depth(10);
    let mut in_loaded = 0usize;
    let mut halo = 0usize;
    for n in net.hash.values() {
        let shard = strategy.locate(n.position);
        if net.loaded.contains(&shard) {
            in_loaded += 1;
        } else {
            halo += 1;
        }
    }
    assert!(in_loaded > 0);
    // Halo is expected but should be a small fraction of the total.
    assert!(
        halo < in_loaded,
        "halo {halo} dwarfed selection {in_loaded}"
    );
}

#[test]
fn network_trait_is_implemented() {
    let (net, _) = build(SelectionMode::Owned);
    let first_node = *net.hash.keys().next().expect("should have nodes");
    assert!(net.point(&first_node).is_some());
}
