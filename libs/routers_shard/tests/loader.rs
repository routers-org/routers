//! Loader-side tests on native — exercises `FileShardFetcher` and the
//! generic `ShardLoader<…, FileShardFetcher>` round-trip end-to-end.

mod common;

use common::MemSource;
use geo::Point;
use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_shard::{
    FileShardFetcher, QuadKey, QuadTreeStrategy, Selection, SelectionMode, ShardLoader,
    ShardedNetwork, ShardingStrategy,
};

fn naming(key: &QuadKey) -> String {
    format!("d{}_{}.shard.rt", key.depth, key.bits)
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("routers_shard_loader_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("mk tmp");
    p
}

fn write_one_shard(dir: &std::path::Path) -> QuadKey {
    let source = MemSource::grid(Point::new(0.0, 0.0), 4, 4, 0.01);
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    let selection = Selection::new(&strategy, owned, SelectionMode::Owned);
    let net = ShardedNetwork::<OsmEntryId, OsmEdgeMetadata, QuadKey>::from_source(
        &source, &strategy, &selection,
    )
    .expect("build");
    net.save_to_file(&dir.join(naming(&owned))).expect("save");
    owned
}

#[tokio::test]
async fn loader_round_trips_via_file_fetcher() {
    let dir = temp_dir("round_trip");
    let owned = write_one_shard(&dir);

    let fetcher = FileShardFetcher::new(&dir);
    let mut loader =
        ShardLoader::<OsmEntryId, OsmEdgeMetadata, QuadKey, _, _>::new(fetcher, naming);

    assert!(loader.get(&owned).is_none());
    let net = loader.load(&owned).await.expect("load");
    assert!(net.num_nodes() > 0);
    assert_eq!(net.owned, owned);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn second_load_hits_the_cache() {
    let dir = temp_dir("cache_hit");
    let owned = write_one_shard(&dir);

    let fetcher = FileShardFetcher::new(&dir);
    let mut loader =
        ShardLoader::<OsmEntryId, OsmEdgeMetadata, QuadKey, _, _>::new(fetcher, naming);

    let a = loader.load(&owned).await.expect("first");
    // Delete the file — second load should still succeed from cache.
    let _ = std::fs::remove_dir_all(&dir);
    let b = loader.load(&owned).await.expect("second");
    assert!(
        std::sync::Arc::ptr_eq(&a, &b),
        "expected same Arc on cache hit"
    );
}

#[tokio::test]
async fn missing_file_surfaces_fetch_error() {
    let dir = temp_dir("missing");
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    let fetcher = FileShardFetcher::new(&dir);
    let mut loader =
        ShardLoader::<OsmEntryId, OsmEdgeMetadata, QuadKey, _, _>::new(fetcher, naming);
    let err = loader.load(&owned).await.expect_err("must fail");
    let s = format!("{err:?}");
    assert!(s.contains("Fetch"), "expected Fetch variant, got {s}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn corrupt_blob_surfaces_decode_error() {
    let dir = temp_dir("corrupt");
    let strategy = QuadTreeStrategy::with_depth(1);
    let owned = strategy.locate(Point::new(0.01, 0.01));
    std::fs::write(dir.join(naming(&owned)), b"not a shard").expect("write");

    let fetcher = FileShardFetcher::new(&dir);
    let mut loader =
        ShardLoader::<OsmEntryId, OsmEdgeMetadata, QuadKey, _, _>::new(fetcher, naming);
    let err = loader.load(&owned).await.expect_err("must fail");
    let s = format!("{err:?}");
    assert!(s.contains("Decode"), "expected Decode variant, got {s}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn load_many_loads_all() {
    let dir = temp_dir("many");
    let owned = write_one_shard(&dir);
    let fetcher = FileShardFetcher::new(&dir);
    let mut loader =
        ShardLoader::<OsmEntryId, OsmEdgeMetadata, QuadKey, _, _>::new(fetcher, naming);
    loader
        .load_many(std::iter::once(owned))
        .await
        .expect("load many");
    assert_eq!(loader.cache().len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
