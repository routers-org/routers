//! Tests for [`Selection`] under both strategies — verifies that the
//! selection abstraction is genuinely strategy-agnostic.

use geo::Point;
use routers_shard::{
    GeohashStrategy, QuadTreeStrategy, Selection, SelectionMode, ShardingStrategy,
};

#[test]
fn owned_mode_loads_single_shard_quadtree() {
    let strategy = QuadTreeStrategy::with_depth(8);
    let owned = strategy.locate(Point::new(13.4, 52.5));
    let sel = Selection::new(&strategy, owned, SelectionMode::Owned);
    assert_eq!(sel.loaded.len(), 1);
    assert!(sel.contains(&owned));
    assert_eq!(sel.owned, owned);
}

#[test]
fn owned_mode_loads_single_shard_geohash() {
    let strategy = GeohashStrategy::with_precision(6);
    let owned = strategy.locate(Point::new(13.4, 52.5));
    let sel = Selection::new(&strategy, owned.clone(), SelectionMode::Owned);
    assert_eq!(sel.loaded.len(), 1);
    assert!(sel.contains(&owned));
}

#[test]
fn neighbour_mode_loads_owned_plus_all_neighbours() {
    let strategy = QuadTreeStrategy::with_depth(8);
    let owned = strategy.locate(Point::new(13.4, 52.5));
    let neighbours = strategy.neighbours(&owned);
    let sel = Selection::new(&strategy, owned, SelectionMode::OwnedAndNeighbours);
    assert_eq!(sel.loaded.len(), neighbours.len() + 1);
    assert!(sel.contains(&owned));
    for n in &neighbours {
        assert!(sel.contains(n), "selection missing neighbour {n:?}");
    }
}

#[test]
fn neighbour_mode_at_boundary_drops_to_fewer_neighbours() {
    let strategy = QuadTreeStrategy::with_depth(8);
    // Near the south pole — fewer than 8 neighbours should be loaded.
    let owned = strategy.locate(Point::new(0.0, -89.99));
    let sel = Selection::new(&strategy, owned, SelectionMode::OwnedAndNeighbours);
    assert!(
        sel.loaded.len() < 9,
        "expected boundary cell to load fewer shards"
    );
    assert!(
        sel.loaded.len() >= 2,
        "should at least include some neighbours"
    );
}

#[test]
fn selection_contains_only_loaded() {
    let strategy = QuadTreeStrategy::with_depth(6);
    let owned = strategy.locate(Point::new(13.4, 52.5));
    let sel = Selection::new(&strategy, owned, SelectionMode::Owned);
    // A neighbour cell should *not* be in the owned-only selection.
    let neighbour = strategy.neighbours(&owned).into_iter().next().unwrap();
    assert!(!sel.contains(&neighbour));
}
