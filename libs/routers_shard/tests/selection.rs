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
fn padded_mode_loaded_set_is_owned_only() {
    // OwnedAndPadded never adds whole neighbour shards: the padded
    // strip is carried as geometry, not as extra shard ids.
    let strategy = GeohashStrategy::with_precision(3);
    let owned = strategy.locate(Point::new(13.4, 52.5));
    let sel = Selection::new(
        &strategy,
        owned,
        SelectionMode::OwnedAndPadded {
            padding_distance: 50.0,
        },
    );
    assert_eq!(sel.loaded.len(), 1);
    assert!(sel.contains(&owned));
    assert!(sel.padding.is_some());
}

#[test]
fn padded_buffer_extends_beyond_owned_bounds() {
    let strategy = GeohashStrategy::with_precision(6);
    let owned = strategy.locate(Point::new(13.4, 52.5));
    let cell = strategy.bounds(&owned);
    let sel = Selection::new(
        &strategy,
        owned,
        SelectionMode::OwnedAndPadded {
            padding_distance: 50.0,
        },
    );
    let padding = sel.padding.expect("padded mode sets a buffer");
    // The buffer encloses the cell strictly.
    assert!(padding.min().x < cell.min().x);
    assert!(padding.max().x > cell.max().x);
    assert!(padding.min().y < cell.min().y);
    assert!(padding.max().y > cell.max().y);

    // A point well inside the cell is in the padding region; a point
    // far outside it is not.
    let centre = Point::new(
        0.5 * (cell.min().x + cell.max().x),
        0.5 * (cell.min().y + cell.max().y),
    );
    assert!(sel.padding_contains(centre));
    assert!(!sel.padding_contains(Point::new(centre.x() + 10.0, centre.y())));
}

#[test]
fn padding_contains_false_when_no_buffer() {
    let strategy = QuadTreeStrategy::with_depth(8);
    let owned = strategy.locate(Point::new(13.4, 52.5));
    let sel = Selection::new(&strategy, owned, SelectionMode::Owned);
    assert!(!sel.padding_contains(Point::new(13.4, 52.5)));
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
