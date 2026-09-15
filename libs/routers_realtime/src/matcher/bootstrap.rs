//! Matcher graph bootstrap: turn a catalog entry into a solvable network. (T09)
//!
//! A matcher serves exactly one [`Region`] and, before it may pull a single
//! job, it has to hold that region's road network in memory and prove the
//! bytes it loaded are the exact, intact artifact the catalog names. Loading a
//! truncated or wrong-version graph would make the process solve silently
//! against a broken network, so bootstrap does the whole dance up front, once,
//! and only reports [`ReadyState::Ready`] when the graph is actually usable:
//!
//! 1. Publish [`ReadyState::Starting`] so a readiness probe steers traffic away
//!    while we load.
//! 2. Load and validate the [`Catalog`] and pick out `--region`.
//! 3. Load the artifact [`Manifest`] beside the shard bundles and **verify
//!    every coverage cell before loading any of them** — a bad checksum on the
//!    last cell must fail the whole boot, not after we have already spent the
//!    time and memory decoding the earlier ones.
//! 4. Decode each verified `.shard.rt` on the blocking pool (decoding a
//!    multi-hundred-megabyte bundle is CPU- and IO-bound, never something to do
//!    on the async reactor) and compose them.
//! 5. Publish [`ReadyState::Ready`] on success or [`ReadyState::Failed`] on any
//!    error, so the failure is visible to the health endpoint rather than a
//!    silent half-loaded process.
//!
//! # The composed network type
//!
//! A region owns one or more geohash cells, one shard bundle each. The fleet's
//! graph type is [`ShardedNetwork<OsmEntryId, OsmEdgeMetadata, Geohash>`]
//! ([`Shard`]), and [`routers_shard::MultiShardNetwork`] aggregates several of
//! them behind the same [`routers_network`] traits (`DataPlane + Scan + Route +
//! Debug + Send + Sync`), which is exactly `routers_network::Network` — so a
//! `Matcher` can solve on it. Rather than carry two network types (a bare
//! [`Shard`] for one cell, a composite for many) behind an enum whose only job
//! would be to hand-delegate every trait method, [`Net`] is *uniformly* the
//! composite: a single-cell region is a one-shard [`MultiShardNetwork`]. That
//! keeps one concrete `Net` for every region at the cost of one graph-copy at
//! startup, which is paid once and never on the hot path. See the module's
//! deviation note in the task report.

use alloc::sync::Arc;
use std::collections::HashSet;
use std::path::PathBuf;

use routers_codec::osm::{OsmEdgeMetadata, OsmEntryId};
use routers_shard::{Geohash, MultiShardNetwork, ShardedNetwork};
use thiserror::Error;
use tokio::task::JoinError;
use tokio::time::Instant;
use tracing::info;

use crate::lifecycle::{ReadinessSetter, ReadyState};
use crate::protocol::ids::RegionId;
use crate::region::{ArtifactError, Catalog, CatalogError, Manifest, Region};

/// One verified shard bundle, decoded from its `.shard.rt` file. This is the
/// fleet's graph type today: OSM node ids, OSM edge metadata, sharded by
/// geohash cell.
pub type Shard = ShardedNetwork<OsmEntryId, OsmEdgeMetadata, Geohash>;

/// The composed network a [`Matcher`](routers_transition::Matcher) solves
/// against. Always a [`MultiShardNetwork`], even for a single-cell region (see
/// the module docs), so every region yields one concrete network type.
pub type Net = MultiShardNetwork<OsmEntryId, OsmEdgeMetadata, Geohash>;

/// Where to find the pieces a matcher needs at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BootstrapConfig {
    /// Path to the region catalog TOML (mounted read-only).
    pub catalog: PathBuf,
    /// The region this matcher serves; must exist in the catalog.
    pub region: RegionId,
    /// Directory holding the `manifest.json` and the `.shard.rt` bundles.
    pub shard_dir: PathBuf,
}

/// A fully loaded, verified region graph: everything a matcher needs to start
/// pulling jobs.
#[derive(Debug, Clone)]
pub struct Loaded {
    /// The region this matcher serves, as validated in the catalog.
    pub region: Region,
    /// The catalog snapshot version the graph was loaded against, so a stale
    /// mount can be detected downstream.
    pub catalog_version: u64,
    /// The catalog's routing version, so results computed here are never mixed
    /// with results from a different point-to-region mapping.
    pub routing_version: u64,
    /// The composed, solvable network. Shared (`Arc`) across the whole pool of
    /// blocking solves.
    pub network: Arc<Net>,
    /// Every cell this region can serve: its owned `coverage` cells plus its
    /// certified `overlap` fallbacks. Used by [`serves`](Self::serves).
    pub cells: HashSet<Geohash>,
}

impl Loaded {
    /// Whether this region will serve solves for `cell` — true for an owned
    /// coverage cell or a certified overlap fallback. Membership only; a served
    /// overlap cell need not have its own loaded bundle, because the coverage
    /// shards already cover the boundary.
    #[must_use]
    pub fn serves(&self, cell: &Geohash) -> bool {
        self.cells.contains(cell)
    }
}

/// Everything graph bootstrap can fail on. Each variant carries enough to point
/// an operator at the offending catalog entry, cell, or bundle.
#[derive(Debug, Error)]
pub enum BootstrapError {
    /// The catalog could not be read or was invalid.
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogError),
    /// The catalog parsed, but held no region with the requested id.
    #[error("catalog has no region {0:?}")]
    UnknownRegion(RegionId),
    /// The manifest was missing/invalid, or a coverage cell's artifact failed
    /// verification (wrong graph, wrong size, bad checksum, ...).
    #[error("artifact: {0}")]
    Artifact(#[from] ArtifactError),
    /// A verified bundle could not be decoded into a network (corrupt bytes, or
    /// a shard-format version this build does not understand).
    ///
    /// The cause is a plain `String` (that is all `routers_shard` returns) held
    /// in `reason` rather than a `source` field, because `thiserror` reserves a
    /// field named `source` for a nested [`std::error::Error`].
    #[error("failed to load shard for cell {cell:?}: {reason}")]
    Load {
        /// The coverage cell whose bundle failed to decode.
        cell: String,
        /// The underlying decode error, rendered as text.
        reason: String,
    },
    /// The region owns more than one cell and the composite network could not
    /// be used as a solvable [`Network`](routers_network::Network). Not reachable
    /// today — [`MultiShardNetwork`] *is* a `Network`, so multi-cell regions
    /// compose fine — but kept as the contract for a future composite that is
    /// not directly solvable.
    #[error(
        "region {region:?} owns multiple cells {cells:?} that cannot be composed into a solvable network"
    )]
    MultiCellUnsupported {
        /// The region that could not be composed.
        region: RegionId,
        /// Its coverage cells, as canonical geohash strings.
        cells: Vec<String>,
    },
    /// The blocking shard-decode task panicked or was cancelled.
    #[error("shard load task failed to join: {0}")]
    Join(JoinError),
}

/// Load and verify the region graph named by `cfg`, publishing readiness
/// transitions to `readiness` as it goes.
///
/// Sets [`ReadyState::Starting`] on entry, [`ReadyState::Ready`] once the graph
/// is composed and usable, and [`ReadyState::Failed`] on any error (the error
/// is both published as `Failed` and returned). Graph loading happens here,
/// once, at startup — never per observation.
///
/// # Errors
///
/// Returns a [`BootstrapError`] if the catalog is missing/invalid, the region
/// is unknown, the manifest or any coverage artifact fails verification, or a
/// bundle cannot be decoded.
pub async fn bootstrap(
    cfg: &BootstrapConfig,
    readiness: &ReadinessSetter,
) -> Result<Loaded, BootstrapError> {
    readiness.set(ReadyState::Starting);
    match load(cfg).await {
        Ok(loaded) => {
            readiness.set(ReadyState::Ready);
            Ok(loaded)
        }
        Err(err) => {
            // Publish the failure before returning so a health endpoint sees a
            // `Failed` process rather than one stuck in `Starting`.
            readiness.set(ReadyState::Failed);
            Err(err)
        }
    }
}

/// The load itself, split out so [`bootstrap`] owns the single readiness
/// state-machine and every early return here funnels through one `Failed`.
async fn load(cfg: &BootstrapConfig) -> Result<Loaded, BootstrapError> {
    let started = Instant::now();

    let catalog = Catalog::load(&cfg.catalog)?;
    let region = catalog
        .region(&cfg.region)
        .ok_or_else(|| BootstrapError::UnknownRegion(cfg.region.clone()))?
        .clone();

    let manifest = Manifest::load(&cfg.shard_dir)?;

    // Verify every coverage cell BEFORE decoding any of them: a bad checksum on
    // the last cell should fail fast, not after we have paid to load the rest.
    let mut verified = Vec::with_capacity(region.coverage.len());
    for cell in &region.coverage {
        let artifact = manifest.verify(&cfg.shard_dir, cell, &region.graph)?;
        verified.push((*cell, artifact));
    }

    // Now decode each verified bundle on the blocking pool.
    let mut shards: Vec<Arc<Shard>> = Vec::with_capacity(verified.len());
    for (cell, artifact) in &verified {
        let path = artifact.path.clone();
        let shard = tokio::task::spawn_blocking(move || Shard::from_cached(&path))
            .await
            .map_err(BootstrapError::Join)?
            .map_err(|reason| BootstrapError::Load {
                cell: cell.to_string(),
                reason,
            })?;
        info!(
            cell = %cell,
            bytes = artifact.artifact.bytes,
            nodes = artifact.artifact.nodes,
            edges = artifact.artifact.edges,
            "loaded shard bundle",
        );
        shards.push(Arc::new(shard));
    }

    // A single-cell region is a one-shard composite; a multi-cell region
    // composes them all. Either way `Net` is one concrete `MultiShardNetwork`.
    let network = MultiShardNetwork::new(shards);

    let mut cells: HashSet<Geohash> = region.coverage.iter().copied().collect();
    cells.extend(region.overlap.iter().copied());

    info!(
        region = %region.id,
        graph = %region.graph,
        cells = verified.len(),
        served = cells.len(),
        nodes = network.num_nodes(),
        edges = network.num_edges(),
        elapsed = ?started.elapsed(),
        "region graph ready",
    );

    Ok(Loaded {
        region,
        catalog_version: catalog.version,
        routing_version: catalog.routing_version,
        network: Arc::new(network),
        cells,
    })
}

#[cfg(test)]
mod tests {
    use alloc::collections::BTreeMap;
    use core::str::FromStr;
    use core::sync::atomic::{AtomicU64, Ordering};
    use core::task::Poll;

    use geo::Point;
    use routers_network::edge::Weight;
    use routers_shard::{GeohashStrategy, Selection, SelectionMode, ShardSource};

    use super::*;
    use crate::event::SHARD_PRECISION;
    use crate::lifecycle::Readiness;
    use crate::protocol::ids::GraphVersion;
    use crate::region::artifact::{Artifact, MANIFEST_FILENAME};

    const GRAPH: &str = "test-graph";

    /// A shard source with no nodes and no edges. Building an empty
    /// [`ShardedNetwork`] from it needs no `OsmEntryId`/`OsmEdgeMetadata`
    /// values, so a test can produce a genuine, loadable `.shard.rt` bundle
    /// without any OSM fixture data.
    struct EmptySource;

    impl ShardSource<OsmEntryId, OsmEdgeMetadata> for EmptySource {
        fn nodes<'a>(&'a self) -> Box<dyn Iterator<Item = (OsmEntryId, Point)> + 'a> {
            Box::new(core::iter::empty())
        }

        fn edges<'a>(
            &'a self,
        ) -> Box<dyn Iterator<Item = (OsmEntryId, OsmEntryId, Weight, OsmEdgeMetadata)> + 'a>
        {
            Box::new(core::iter::empty())
        }
    }

    /// A unique directory under the temp dir that does not yet exist,
    /// namespaced by process id and a per-call counter so parallel tests never
    /// collide without reaching for a wall clock.
    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "routers-bootstrap-{tag}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        path
    }

    fn cell(s: &str) -> Geohash {
        Geohash::from_str(s).unwrap()
    }

    /// Write a real, loadable empty shard bundle for `cell` into `dir` and
    /// return the manifest [`Artifact`] describing it (checksum and size taken
    /// from the bytes actually written, so `verify` passes).
    fn write_shard(dir: &std::path::Path, cell: &str) -> Artifact {
        let file = format!("{cell}.shard.rt");
        let path = dir.join(&file);

        let strategy = GeohashStrategy::with_precision(SHARD_PRECISION);
        let selection = Selection::new(
            &strategy,
            Geohash::from_str(cell).unwrap(),
            SelectionMode::Owned,
        );
        let net =
            Shard::from_source(&EmptySource, &strategy, &selection).expect("build empty shard");
        net.save_to_file(&path).expect("save shard bundle");

        Artifact {
            file,
            sha256: crate::region::sha256_hex(&path).expect("hash bundle"),
            bytes: std::fs::metadata(&path).expect("stat bundle").len(),
            graph: GraphVersion::new(GRAPH).unwrap(),
            precision: SHARD_PRECISION,
            nodes: 0,
            edges: 0,
            built_at: "2026-09-14T00:00:00Z".to_owned(),
        }
    }

    fn write_manifest(dir: &std::path::Path, artifacts: BTreeMap<String, Artifact>) {
        let manifest = Manifest {
            version: 1,
            artifacts,
        };
        std::fs::write(
            dir.join(MANIFEST_FILENAME),
            serde_json::to_string(&manifest).unwrap(),
        )
        .expect("write manifest");
    }

    fn write_catalog(dir: &std::path::Path, coverage: &[&str], overlap: &[&str]) -> PathBuf {
        let list = |cells: &[&str]| {
            cells
                .iter()
                .map(|c| format!("\"{c}\""))
                .collect::<Vec<_>>()
                .join(", ")
        };
        let toml = format!(
            "version = 5\nrouting_version = 7\n\n[[regions]]\nid = \"test-region\"\ngraph = \"{GRAPH}\"\ncoverage = [{}]\noverlap = [{}]\nlanes = 1\nresource_class = \"cpu-1\"\nreplicas = {{ min = 1, max = 1 }}\nfreshness_budget_ms = 1000\n",
            list(coverage),
            list(overlap),
        );
        let path = dir.join("catalog.toml");
        std::fs::write(&path, toml).expect("write catalog");
        path
    }

    /// A fully valid environment: shard bundles for `coverage`, a manifest over
    /// them, and a catalog naming `coverage`/`overlap` for `test-region`.
    fn valid_fixture(tag: &str, coverage: &[&str], overlap: &[&str]) -> (PathBuf, BootstrapConfig) {
        let dir = scratch_dir(tag);
        let mut artifacts = BTreeMap::new();
        for c in coverage {
            artifacts.insert((*c).to_owned(), write_shard(&dir, c));
        }
        write_manifest(&dir, artifacts);
        let catalog = write_catalog(&dir, coverage, overlap);
        let cfg = BootstrapConfig {
            catalog,
            region: RegionId::new("test-region").unwrap(),
            shard_dir: dir.clone(),
        };
        (dir, cfg)
    }

    /// The static type-level guarantee the whole module exists to uphold: the
    /// composed `Net` is a `routers_network::Network`, so a `Matcher` can solve
    /// on it.
    #[test]
    fn composed_net_is_a_routing_network() {
        fn assert_network<N: routers_network::Network>() {}
        assert_network::<Net>();
    }

    #[tokio::test]
    async fn single_cell_region_boots_to_ready() {
        let (dir, cfg) = valid_fixture("single", &["r3gq"], &[]);
        let (setter, watcher) = Readiness::new();

        let loaded = bootstrap(&cfg, &setter).await.expect("boot succeeds");

        assert_eq!(watcher.current(), ReadyState::Ready);
        assert_eq!(loaded.region.id.as_str(), "test-region");
        assert_eq!(loaded.catalog_version, 5);
        assert_eq!(loaded.routing_version, 7);
        assert!(loaded.serves(&cell("r3gq")));
        assert!(!loaded.serves(&cell("r3gz")));
        assert_eq!(loaded.cells.len(), 1);
        assert_eq!(loaded.network.shard_count(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn multi_cell_region_composes_and_serves_overlap() {
        // Two owned cells (two bundles, one composite) plus a certified overlap
        // fallback that has no bundle of its own but is still "served".
        let (dir, cfg) = valid_fixture("multi", &["r3gq", "r3gr"], &["r3gw"]);
        let (setter, watcher) = Readiness::new();

        let loaded = bootstrap(&cfg, &setter).await.expect("boot succeeds");

        assert_eq!(watcher.current(), ReadyState::Ready);
        assert_eq!(loaded.network.shard_count(), 2);
        assert!(loaded.serves(&cell("r3gq")));
        assert!(loaded.serves(&cell("r3gr")));
        assert!(loaded.serves(&cell("r3gw")), "overlap cell is served");
        assert_eq!(loaded.cells.len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_catalog_is_a_catalog_error() {
        let dir = scratch_dir("nocatalog");
        let cfg = BootstrapConfig {
            catalog: dir.join("absent.toml"),
            region: RegionId::new("test-region").unwrap(),
            shard_dir: dir.clone(),
        };
        let (setter, watcher) = Readiness::new();

        let err = bootstrap(&cfg, &setter).await.unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::Catalog(CatalogError::Io { .. })
        ));
        assert_eq!(watcher.current(), ReadyState::Failed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn unknown_region_is_reported() {
        let (dir, mut cfg) = valid_fixture("unknown", &["r3gq"], &[]);
        cfg.region = RegionId::new("not-here").unwrap();
        let (setter, watcher) = Readiness::new();

        let err = bootstrap(&cfg, &setter).await.unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::UnknownRegion(id) if id.as_str() == "not-here"
        ));
        assert_eq!(watcher.current(), ReadyState::Failed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn missing_manifest_is_an_artifact_error() {
        // Valid catalog, but the shard directory holds no manifest.
        let dir = scratch_dir("nomanifest");
        let catalog = write_catalog(&dir, &["r3gq"], &[]);
        let cfg = BootstrapConfig {
            catalog,
            region: RegionId::new("test-region").unwrap(),
            shard_dir: dir.clone(),
        };
        let (setter, watcher) = Readiness::new();

        let err = bootstrap(&cfg, &setter).await.unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::Artifact(ArtifactError::MissingManifest { .. })
        ));
        assert_eq!(watcher.current(), ReadyState::Failed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn tampered_bundle_fails_the_checksum() {
        // Build a valid fixture, then flip a byte of the bundle so its recomputed
        // hash no longer matches the manifest the checksum was taken over.
        let (dir, cfg) = valid_fixture("tamper", &["r3gq"], &[]);
        let bundle = dir.join("r3gq.shard.rt");
        let mut bytes = std::fs::read(&bundle).unwrap();
        bytes[0] ^= 0xff; // same length, so the size gate passes and the hash gate trips
        std::fs::write(&bundle, &bytes).unwrap();
        let (setter, watcher) = Readiness::new();

        let err = bootstrap(&cfg, &setter).await.unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::Artifact(ArtifactError::ChecksumMismatch { cell }) if cell == "r3gq"
        ));
        assert_eq!(watcher.current(), ReadyState::Failed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_bad_cell_fails_before_any_bundle_is_loaded() {
        // Two coverage cells: the first bundle is intact, the second's manifest
        // entry names a graph the catalog does not expect. Because every cell is
        // verified before any is decoded, the whole boot fails on the mismatch.
        let dir = scratch_dir("failfast");
        let good = write_shard(&dir, "r3gq");
        let mut bad = write_shard(&dir, "r3gr");
        bad.graph = GraphVersion::new("some-other-graph").unwrap();
        let mut artifacts = BTreeMap::new();
        artifacts.insert("r3gq".to_owned(), good);
        artifacts.insert("r3gr".to_owned(), bad);
        write_manifest(&dir, artifacts);
        let catalog = write_catalog(&dir, &["r3gq", "r3gr"], &[]);
        let cfg = BootstrapConfig {
            catalog,
            region: RegionId::new("test-region").unwrap(),
            shard_dir: dir.clone(),
        };
        let (setter, watcher) = Readiness::new();

        let err = bootstrap(&cfg, &setter).await.unwrap_err();
        assert!(matches!(
            err,
            BootstrapError::Artifact(ArtifactError::GraphMismatch { cell, .. }) if cell == "r3gr"
        ));
        assert_eq!(watcher.current(), ReadyState::Failed);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Readiness must walk `Starting` then `Failed` on a load that fails after
    /// the graph starts decoding. The manifest checksum is taken over a garbage
    /// bundle so verification passes but the decode fails, which suspends the
    /// future on the blocking pool — that suspension is the window in which
    /// `Starting` is observable before the eventual `Failed`.
    #[tokio::test]
    async fn readiness_goes_starting_then_failed() {
        let dir = scratch_dir("transition");
        let file = "r3gq.shard.rt";
        let path = dir.join(file);
        // Large enough that the blocking read is still running at the first poll,
        // and not a valid shard header, so `from_cached` returns an error.
        let garbage = vec![0xAB_u8; 2 << 20];
        std::fs::write(&path, &garbage).unwrap();
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "r3gq".to_owned(),
            Artifact {
                file: file.to_owned(),
                sha256: crate::region::sha256_hex(&path).unwrap(),
                bytes: garbage.len() as u64,
                graph: GraphVersion::new(GRAPH).unwrap(),
                precision: SHARD_PRECISION,
                nodes: 0,
                edges: 0,
                built_at: "2026-09-14T00:00:00Z".to_owned(),
            },
        );
        write_manifest(&dir, artifacts);
        let catalog = write_catalog(&dir, &["r3gq"], &[]);
        let cfg = BootstrapConfig {
            catalog,
            region: RegionId::new("test-region").unwrap(),
            shard_dir: dir.clone(),
        };

        let (setter, watcher) = Readiness::new();
        let fut = bootstrap(&cfg, &setter);
        tokio::pin!(fut);

        // First poll runs synchronously up to the blocking decode, publishing
        // `Starting` and suspending — verifying `Starting` is set before the
        // outcome is known.
        match futures::poll!(fut.as_mut()) {
            Poll::Pending => assert_eq!(watcher.current(), ReadyState::Starting),
            Poll::Ready(_) => panic!("decode should suspend so Starting is observable"),
        }

        let err = fut.await.unwrap_err();
        assert!(matches!(err, BootstrapError::Load { cell, .. } if cell == "r3gq"));
        assert_eq!(watcher.current(), ReadyState::Failed);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
