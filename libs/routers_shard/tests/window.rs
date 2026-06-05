//! Tests for [`ShardWindow`] — the 9-cell sliding cache.

mod common;

use common::MemSource;
use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_shard::{
    FileFetcher, QuadKey, QuadTreeStrategy, Selection, SelectionMode, ShardMoveDelta, ShardWindow,
    ShardedNetwork, ShardingStrategy,
};

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("routers_shard_window_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("mkdir");
    p
}

/// Build a 9-shard neighbourhood around `anchor` and dump each as a file
/// in `dir`. Returns the centre key plus the dir.
fn write_neighbourhood(
    anchor: Point,
    depth: u8,
    tag: &str,
) -> (std::path::PathBuf, QuadTreeStrategy, QuadKey) {
    let dir = temp_dir(tag);
    let strategy = QuadTreeStrategy::with_depth(depth);
    let owned = strategy.locate(anchor);
    // Wide grid so multiple cells are populated.
    let source = MemSource::grid(Point::new(-0.5, -0.5), 32, 32, 0.1);
    for key in std::iter::once(owned).chain(strategy.neighbours(&owned)) {
        let selection = Selection::new(&strategy, key, SelectionMode::Owned);
        let net = ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, QuadKey>::from_source(
            &source, &strategy, &selection,
        )
        .expect("build");
        net.save_to_file(&dir.join(key.to_string())).expect("save");
    }
    (dir, strategy, owned)
}

#[tokio::test]
async fn first_recenter_yields_full_to_fetch_list() {
    let (dir, strategy, owned) = write_neighbourhood(Point::new(0.0, 0.0), 4, "first_recenter");
    let fetcher = FileFetcher::new(&dir);
    let mut window =
        ShardWindow::<OsmEntryId, OsmEdgeMetadata, _, _>::new(strategy.clone(), fetcher);

    let center_point = {
        let r = strategy.bounds(&owned);
        Point::new(0.5 * (r.min().x + r.max().x), 0.5 * (r.min().y + r.max().y))
    };
    let ShardMoveDelta::Recentered {
        scoped: to_fetch,
        unscoped: evicted,
    } = window.recenter(center_point)
    else {
        panic!("expected Recentered delta on first recenter");
    };
    assert_eq!(evicted.len(), 0);
    // owned + 8 neighbours = 9, all not yet loaded.
    assert_eq!(to_fetch.len(), 9);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn fetch_one_populates_cache_and_owned_resolves() {
    let (dir, strategy, owned) = write_neighbourhood(Point::new(0.0, 0.0), 4, "fetch_one");
    let fetcher = FileFetcher::new(&dir);
    let mut window =
        ShardWindow::<OsmEntryId, OsmEdgeMetadata, _, _>::new(strategy.clone(), fetcher);

    let center_point = {
        let r = strategy.bounds(&owned);
        Point::new(0.5 * (r.min().x + r.max().x), 0.5 * (r.min().y + r.max().y))
    };
    let _ = window.recenter(center_point);

    assert!(window.get(&owned).is_none(), "no shards loaded yet");
    window.fetch(&owned).await.expect("fetch");

    let net = window.get(&owned).expect("centre now loaded");
    assert!(net.num_nodes() > 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn pan_to_neighbour_promotes_without_refetch() {
    let (dir, strategy, owned) = write_neighbourhood(Point::new(0.0, 0.0), 4, "pan_to_neighbour");
    let fetcher = FileFetcher::new(&dir);
    let mut window =
        ShardWindow::<OsmEntryId, OsmEdgeMetadata, _, _>::new(strategy.clone(), fetcher);

    // Step 1: recenter on owned, fetch every cell.
    let center_point = {
        let r = strategy.bounds(&owned);
        Point::new(0.5 * (r.min().x + r.max().x), 0.5 * (r.min().y + r.max().y))
    };
    let ShardMoveDelta::Recentered { scoped, .. } = window.recenter(center_point) else {
        panic!("expected recentered");
    };
    for key in &scoped {
        window.fetch(key).await.expect("fetch");
    }
    assert_eq!(window.loaded_ids().len(), 9);

    // Step 2: pan to a neighbour.
    let neighbour = *strategy.neighbours(&owned).first().expect("has neighbours");
    let neighbour_point = {
        let r = strategy.bounds(&neighbour);
        Point::new(0.5 * (r.min().x + r.max().x), 0.5 * (r.min().y + r.max().y))
    };
    let ShardMoveDelta::Recentered { unscoped, .. } = window.recenter(neighbour_point) else {
        panic!("expected recentered");
    };

    // The previously-loaded neighbour is now the centre — already cached, not in scoped.
    assert!(
        !unscoped.contains(&neighbour),
        "expected {:?} to not be in the unscoped set: {:?}",
        neighbour,
        unscoped
    );

    // The new centre is still cached.
    assert!(window.get(&neighbour).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn recenter_to_same_cell_is_a_noop() {
    let (dir, strategy, owned) = write_neighbourhood(Point::new(0.0, 0.0), 4, "recenter_noop");
    let fetcher = FileFetcher::new(&dir);
    let mut window =
        ShardWindow::<OsmEntryId, OsmEdgeMetadata, _, _>::new(strategy.clone(), fetcher);

    let center_point = {
        let r = strategy.bounds(&owned);
        Point::new(0.5 * (r.min().x + r.max().x), 0.5 * (r.min().y + r.max().y))
    };
    let _ = window.recenter(center_point);
    let delta = window.recenter(center_point);

    assert!(
        matches!(delta, ShardMoveDelta::Unchanged),
        "got {:?}, expected Unchanged",
        delta,
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn far_pan_evicts_old_cells() {
    let (dir, strategy, owned) = write_neighbourhood(Point::new(0.0, 0.0), 4, "far_pan");
    let fetcher = FileFetcher::new(&dir);
    let mut window =
        ShardWindow::<OsmEntryId, OsmEdgeMetadata, _, _>::new(strategy.clone(), fetcher);

    let center_point = {
        let r = strategy.bounds(&owned);
        Point::new(0.5 * (r.min().x + r.max().x), 0.5 * (r.min().y + r.max().y))
    };
    let ShardMoveDelta::Recentered { scoped, .. } = window.recenter(center_point) else {
        panic!("expected recentered delta");
    };
    assert!(!scoped.is_empty(), "expected new elements to fetch");

    for key in &scoped {
        window.fetch(key).await.expect("fetch");
    }

    assert_eq!(window.loaded_ids().len(), 9);

    // Pan a long way — far outside the original window. Every old cell
    // should be evicted.
    let ShardMoveDelta::Recentered { unscoped, .. } = window.recenter(Point::new(170.0, 70.0))
    else {
        panic!("expected recentered delta");
    };
    assert!(!unscoped.is_empty(), "expected evictions after far pan");

    for key in &unscoped {
        window.evict(key);
    }

    assert_eq!(
        window.loaded_ids().len(),
        0,
        "no cells should remain cached after a far pan"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
