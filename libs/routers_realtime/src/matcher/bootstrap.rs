//! Matcher graph bootstrap: turn a catalog entry into a solvable network.
//!
//! A matcher serves one [`Region`] and loads its verified graph once at
//! startup. Every coverage cell is verified before any is decoded, so a bad
//! checksum fails the whole boot rather than after paying to load the rest.
//! [`Net`] is always a [`MultiShardNetwork`], even for a single-cell region, so
//! every region yields one concrete network type.

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

/// One verified shard bundle, decoded from its `.shard.rt` file.
pub type Shard = ShardedNetwork<OsmEntryId, OsmEdgeMetadata, Geohash>;

/// The composed network a [`Matcher`](routers_transition::Matcher) solves
/// against. Always a [`MultiShardNetwork`], even for a single-cell region.
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
    /// The catalog snapshot version the graph was loaded against.
    pub catalog_version: u64,
    /// The catalog's routing version.
    pub routing_version: u64,
    /// The composed, solvable network, shared across the pool of blocking solves.
    pub network: Arc<Net>,
    /// Every cell this region serves: owned `coverage` plus certified `overlap`.
    pub cells: HashSet<Geohash>,
}

impl Loaded {
    /// Whether this region will serve solves for `cell` — an owned coverage cell
    /// or a certified overlap fallback.
    #[must_use]
    pub fn serves(&self, cell: &Geohash) -> bool {
        self.cells.contains(cell)
    }
}

/// Everything graph bootstrap can fail on.
#[derive(Debug, Error)]
pub enum BootstrapError {
    /// The catalog could not be read or was invalid.
    #[error("catalog: {0}")]
    Catalog(#[from] CatalogError),
    /// The catalog parsed, but held no region with the requested id.
    #[error("catalog has no region {0:?}")]
    UnknownRegion(RegionId),
    /// The manifest was missing/invalid, or a coverage cell's artifact failed
    /// verification.
    #[error("artifact: {0}")]
    Artifact(#[from] ArtifactError),
    /// A verified bundle could not be decoded into a network.
    #[error("failed to load shard for cell {cell:?}: {reason}")]
    Load {
        /// The coverage cell whose bundle failed to decode.
        cell: String,
        /// The underlying decode error, rendered as text.
        reason: String,
    },
    /// The region owns multiple cells that cannot be composed into a solvable
    /// [`Network`](routers_network::Network). Not reachable today, but kept as
    /// the contract for a future composite that is not directly solvable.
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
/// Sets [`ReadyState::Starting`] on entry, [`ReadyState::Ready`] on success, and
/// [`ReadyState::Failed`] on any error (also returned).
///
/// # Errors
///
/// Returns a [`BootstrapError`] if the catalog, region, manifest, an artifact,
/// or a bundle decode fails.
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
            readiness.set(ReadyState::Failed);
            Err(err)
        }
    }
}

/// The load itself, split out so [`bootstrap`] owns the readiness state-machine.
async fn load(cfg: &BootstrapConfig) -> Result<Loaded, BootstrapError> {
    let started = Instant::now();

    let catalog = Catalog::load(&cfg.catalog)?;
    let region = catalog
        .region(&cfg.region)
        .ok_or_else(|| BootstrapError::UnknownRegion(cfg.region.clone()))?
        .clone();

    let manifest = Manifest::load(&cfg.shard_dir)?;

    // Verify every coverage cell before decoding any, so a bad checksum fails
    // fast rather than after paying to load the rest.
    let mut verified = Vec::with_capacity(region.coverage.len());
    for cell in &region.coverage {
        let artifact = manifest.verify(&cfg.shard_dir, cell, &region.graph)?;
        verified.push((*cell, artifact));
    }

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

    /// A shard source with no nodes and no edges, so a test can build a genuine
    /// loadable `.shard.rt` bundle without any OSM fixture data.
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

    /// A unique scratch directory, namespaced by process id and a per-call
    /// counter so parallel tests never collide.
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

    /// Write a loadable empty shard bundle for `cell` and return the manifest
    /// [`Artifact`] describing it, with checksum and size taken from the bytes
    /// written so `verify` passes.
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

    /// A valid environment: shard bundles for `coverage`, a manifest over them,
    /// and a catalog naming `coverage`/`overlap` for `test-region`.
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
        // The second cell's manifest entry names an unexpected graph; because
        // every cell is verified before any is decoded, the whole boot fails.
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

    #[tokio::test]
    async fn readiness_goes_starting_then_failed() {
        let dir = scratch_dir("transition");
        let file = "r3gq.shard.rt";
        let path = dir.join(file);
        // Large and not a valid shard header, so the blocking decode is still
        // running at the first poll and eventually errors.
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

        // First poll runs up to the blocking decode and suspends, so `Starting`
        // is observable before the outcome is known.
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
