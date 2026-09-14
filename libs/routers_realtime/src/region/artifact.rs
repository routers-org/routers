//! Graph artifact manifests and checksum verification. (T08)
//!
//! A region's road-network snapshot ships as one immutable `.shard.rt` bundle
//! per covered geohash cell, built out-of-band by the offline generator and
//! served from the graph artifact store (GCS in production, a mounted
//! directory in tests). A matcher that loaded a truncated, half-written, or
//! wrong-version bundle would silently solve against a broken network, so the
//! generator writes a **manifest** beside the bundles and every consumer
//! verifies a cell's artifact against it *before* trusting the bytes.
//!
//! The manifest is a small JSON sidecar, keyed by canonical geohash cell, that
//! pins each artifact's file name, size, SHA-256, the [`GraphVersion`] it was
//! built for, its geohash [`precision`](Artifact::precision), and coarse
//! node/edge counts plus a build timestamp for provenance. [`verify`] refuses
//! the artifact unless the cell is present, the graph matches what the caller
//! expects, the file exists, its size and checksum match, and its precision is
//! the fleet-wide [`SHARD_PRECISION`]. Verification is the only supported way
//! to turn a manifest entry into a path a matcher may load.
//!
//! # Format
//!
//! ```json
//! {
//!   "version": 1,
//!   "artifacts": {
//!     "r3gr": {
//!       "file": "r3gr.shard.rt",
//!       "sha256": "<64 hex>",
//!       "bytes": 123456,
//!       "graph": "sydney-2026-09-01",
//!       "precision": 4,
//!       "nodes": 10234,
//!       "edges": 20456,
//!       "built_at": "2026-09-14T00:00:00Z"
//!     }
//!   }
//! }
//! ```
//!
//! One output directory is shared by every region generated into it, so the
//! generator [`merge`](Manifest::merge)s freshly built entries into whatever
//! manifest already exists (newest-wins per cell) rather than replacing it.

// `core::io::ErrorKind` is unstable (feature `core_io`), so `std::io::ErrorKind`
// is the only stable path and the `std_instead_of_core` suggestion cannot be
// followed here.
#![allow(clippy::std_instead_of_core)]

use alloc::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use routers_shard::Geohash;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::event::SHARD_PRECISION;
use crate::protocol::ids::GraphVersion;

/// The manifest's file name, written and read beside the `.shard.rt` bundles.
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// The manifest sidecar: a versioned, cell-keyed index of every graph artifact
/// in a directory.
///
/// Construct one with [`parse`](Self::parse) or [`load`](Self::load); the
/// fields are public for inspection and for the generator to assemble, but a
/// consumer should reach for [`verify`](Self::verify) rather than trusting an
/// entry directly.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// The manifest schema version, so a consumer can detect a format it does
    /// not understand. `1` today.
    pub version: u32,
    /// The artifacts, keyed by canonical geohash cell string (e.g. `"r3gr"`).
    #[serde(default)]
    pub artifacts: BTreeMap<String, Artifact>,
}

/// One immutable graph bundle's metadata: enough to locate the file and prove
/// it is the exact, intact artifact a region expects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    /// The bundle's file name, relative to the manifest's directory.
    pub file: String,
    /// Lowercase hex SHA-256 of the bundle's bytes.
    pub sha256: String,
    /// The bundle's size in bytes; a fast integrity pre-check before hashing.
    pub bytes: u64,
    /// The road-network snapshot this bundle was built from.
    pub graph: GraphVersion,
    /// The geohash precision the bundle's cell was cut at; must equal the
    /// fleet-wide [`SHARD_PRECISION`].
    pub precision: u8,
    /// Node count, for provenance and capacity planning (not verified).
    pub nodes: u64,
    /// Edge count, for provenance and capacity planning (not verified).
    pub edges: u64,
    /// When the bundle was built, as an RFC 3339 UTC timestamp. Used as the
    /// newest-wins tie-break in [`Manifest::merge`]; its lexical order matches
    /// chronological order for this format.
    pub built_at: String,
}

/// An artifact that passed every [`Manifest::verify`] check: the resolved path
/// a matcher may now load, paired with the metadata it was checked against.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedArtifact {
    /// The verified bundle's absolute-or-relative path (the manifest directory
    /// joined with the artifact's file name).
    pub path: PathBuf,
    /// The manifest entry the file was verified against.
    pub artifact: Artifact,
}

/// Everything an artifact can be rejected for. Each variant names the offending
/// cell (and path, where relevant) so an operator can trace it to a bundle.
#[derive(Debug, Error)]
pub enum ArtifactError {
    /// No `manifest.json` existed in the directory.
    #[error("no {MANIFEST_FILENAME} in {dir:?}")]
    MissingManifest {
        /// The directory that held no manifest.
        dir: PathBuf,
    },
    /// The manifest bytes were not valid JSON, or did not match the schema.
    #[error("failed to parse manifest JSON: {0}")]
    Parse(#[from] serde_json::Error),
    /// The manifest had no artifact for the requested cell.
    #[error("manifest has no artifact for cell {cell:?}")]
    MissingCell {
        /// The requested cell, as its canonical geohash string.
        cell: String,
    },
    /// The artifact was built for a different graph than the caller expected.
    #[error("cell {cell:?} was built for graph {got:?}, expected {expected:?}")]
    GraphMismatch {
        /// The cell whose artifact was consulted.
        cell: String,
        /// The graph the caller required.
        expected: String,
        /// The graph the manifest recorded.
        got: String,
    },
    /// The bundle file named by the manifest did not exist on disk.
    #[error("artifact file {path:?} does not exist")]
    MissingFile {
        /// The path that was expected to hold the bundle.
        path: PathBuf,
    },
    /// The bundle file's size did not match the manifest, so it is truncated
    /// or overwritten; the checksum is not even attempted.
    #[error("artifact {cell:?} at {path:?} is {actual} bytes, manifest says {expected}")]
    SizeMismatch {
        /// The cell whose bundle was checked.
        cell: String,
        /// The path that was sized.
        path: PathBuf,
        /// The size the manifest recorded.
        expected: u64,
        /// The size the file actually had.
        actual: u64,
    },
    /// The bundle's SHA-256 did not match the manifest: the bytes are corrupt.
    #[error("artifact {cell:?} checksum does not match the manifest")]
    ChecksumMismatch {
        /// The cell whose bundle failed the checksum.
        cell: String,
    },
    /// The artifact's geohash precision was not the fleet-wide value, so it
    /// names a cell at the wrong scale.
    #[error("artifact {cell:?} has precision {actual}, expected {expected}")]
    PrecisionMismatch {
        /// The cell whose artifact carried the wrong precision.
        cell: String,
        /// The precision every artifact must use ([`SHARD_PRECISION`]).
        expected: u8,
        /// The precision the manifest recorded.
        actual: u8,
    },
    /// Reading the manifest or a bundle failed for a reason other than absence.
    #[error("i/o error on {path:?}: {source}")]
    Io {
        /// The path being read when the failure occurred.
        path: PathBuf,
        /// The underlying I/O failure.
        source: std::io::Error,
    },
}

impl Manifest {
    /// Parse and validate a manifest from a JSON string.
    pub fn parse(json: &str) -> Result<Self, ArtifactError> {
        Ok(serde_json::from_str(json)?)
    }

    /// Read `dir/manifest.json` and [`parse`](Self::parse) it. A missing file
    /// is reported as [`ArtifactError::MissingManifest`], distinct from a
    /// content or I/O failure, so a caller can tell an unpopulated mount from a
    /// broken one.
    pub fn load(dir: impl AsRef<Path>) -> Result<Self, ArtifactError> {
        let dir = dir.as_ref();
        let path = dir.join(MANIFEST_FILENAME);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(ArtifactError::MissingManifest {
                    dir: dir.to_path_buf(),
                });
            }
            Err(source) => return Err(ArtifactError::Io { path, source }),
        };
        Self::parse(&text)
    }

    /// Fold `other`'s entries into this manifest, newest-wins per cell: an
    /// incoming artifact replaces an existing one for the same cell unless the
    /// existing one has a strictly newer `built_at` (ties resolve to `other`,
    /// the incoming batch). The higher `version` of the two is kept. This is
    /// how the generator adds freshly built shards to a directory shared by
    /// several regions without dropping the ones it did not rebuild.
    pub fn merge(&mut self, other: Manifest) {
        self.version = self.version.max(other.version);
        for (cell, artifact) in other.artifacts {
            let keep_existing = self
                .artifacts
                .get(&cell)
                .is_some_and(|existing| existing.built_at > artifact.built_at);
            if !keep_existing {
                self.artifacts.insert(cell, artifact);
            }
        }
    }

    /// Verify the artifact for `cell` against the bundles in `dir` and return
    /// the path a matcher may then load.
    ///
    /// The checks, in order: the cell is present; its `graph` equals
    /// `expected_graph`; its `precision` is [`SHARD_PRECISION`]; the file
    /// exists; its size matches `bytes`; its SHA-256 matches `sha256`. The two
    /// cheap manifest-field checks run before the filesystem is touched, so a
    /// graph or precision mismatch never pays for a hash. The first failed
    /// check is returned.
    pub fn verify(
        &self,
        dir: impl AsRef<Path>,
        cell: &Geohash,
        expected_graph: &GraphVersion,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        let dir = dir.as_ref();
        let key = cell.to_string();

        let artifact = self
            .artifacts
            .get(&key)
            .ok_or_else(|| ArtifactError::MissingCell { cell: key.clone() })?;

        if &artifact.graph != expected_graph {
            return Err(ArtifactError::GraphMismatch {
                cell: key,
                expected: expected_graph.to_string(),
                got: artifact.graph.to_string(),
            });
        }

        if artifact.precision != SHARD_PRECISION {
            return Err(ArtifactError::PrecisionMismatch {
                cell: key,
                expected: SHARD_PRECISION,
                actual: artifact.precision,
            });
        }

        let path = dir.join(&artifact.file);
        let metadata = match std::fs::metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(ArtifactError::MissingFile { path });
            }
            Err(source) => return Err(ArtifactError::Io { path, source }),
        };

        if metadata.len() != artifact.bytes {
            return Err(ArtifactError::SizeMismatch {
                cell: key,
                path,
                expected: artifact.bytes,
                actual: metadata.len(),
            });
        }

        let digest = sha256_hex(&path).map_err(|source| ArtifactError::Io {
            path: path.clone(),
            source,
        })?;
        if !digest.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(ArtifactError::ChecksumMismatch { cell: key });
        }

        Ok(VerifiedArtifact {
            path,
            artifact: artifact.clone(),
        })
    }
}

/// The lowercase hex SHA-256 of the file at `path`, streamed through a 1 MiB
/// buffer so a multi-hundred-megabyte artifact is never resident in memory at
/// once.
pub fn sha256_hex(path: impl AsRef<Path>) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

/// Render bytes as a lowercase hex string, two chars per byte.
fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use core::str::FromStr;
    use core::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    /// The example from the module docs, kept in step with them. The checksum
    /// is a placeholder (64 zeros): parsing does not verify it.
    const EXAMPLE: &str = r#"
{
  "version": 1,
  "artifacts": {
    "r3gr": {
      "file": "r3gr.shard.rt",
      "sha256": "0000000000000000000000000000000000000000000000000000000000000000",
      "bytes": 123456,
      "graph": "sydney-2026-09-01",
      "precision": 4,
      "nodes": 10234,
      "edges": 20456,
      "built_at": "2026-09-14T00:00:00Z"
    }
  }
}
"#;

    fn cell(s: &str) -> Geohash {
        Geohash::from_str(s).unwrap()
    }

    fn graph(s: &str) -> GraphVersion {
        GraphVersion::new(s).unwrap()
    }

    /// A unique directory under the temp dir that does not yet exist,
    /// namespaced by process id and a per-call counter so parallel tests never
    /// collide without reaching for a wall clock.
    fn scratch_dir(tag: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "routers-artifact-{tag}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("create scratch dir");
        path
    }

    /// Build a directory holding one real bundle and a manifest whose
    /// `sha256`/`bytes` are computed from that bundle, so `verify` passes. The
    /// closure lets a test tamper with the bundle bytes or the manifest entry
    /// before the manifest is returned.
    fn fixture(tag: &str, bundle: &[u8], edit: impl FnOnce(&mut Artifact)) -> (PathBuf, Manifest) {
        let dir = scratch_dir(tag);
        let file = "r3gr.shard.rt";
        let path = dir.join(file);
        std::fs::write(&path, bundle).expect("write bundle");

        let mut artifact = Artifact {
            file: file.to_owned(),
            sha256: sha256_hex(&path).expect("hash bundle"),
            bytes: bundle.len() as u64,
            graph: graph("sydney-2026-09-01"),
            precision: SHARD_PRECISION,
            nodes: 10,
            edges: 20,
            built_at: "2026-09-14T00:00:00Z".to_owned(),
        };
        edit(&mut artifact);

        let mut artifacts = BTreeMap::new();
        artifacts.insert("r3gr".to_owned(), artifact);
        (
            dir,
            Manifest {
                version: 1,
                artifacts,
            },
        )
    }

    #[test]
    fn documented_example_parses() {
        let manifest = Manifest::parse(EXAMPLE).expect("documented example should parse");
        assert_eq!(manifest.version, 1);
        assert_eq!(manifest.artifacts.len(), 1);

        let artifact = manifest.artifacts.get("r3gr").expect("cell present");
        assert_eq!(artifact.file, "r3gr.shard.rt");
        assert_eq!(artifact.bytes, 123_456);
        assert_eq!(artifact.graph.as_str(), "sydney-2026-09-01");
        assert_eq!(artifact.precision, 4);
        assert_eq!(artifact.nodes, 10_234);
        assert_eq!(artifact.edges, 20_456);
        assert_eq!(artifact.built_at, "2026-09-14T00:00:00Z");
    }

    #[test]
    fn round_trips_through_json() {
        let manifest = Manifest::parse(EXAMPLE).unwrap();
        let rendered = serde_json::to_string(&manifest).expect("serialise");
        let reparsed = Manifest::parse(&rendered).expect("re-render should parse");
        assert_eq!(manifest, reparsed);
    }

    #[test]
    fn parse_rejects_non_json() {
        assert!(matches!(
            Manifest::parse("not json"),
            Err(ArtifactError::Parse(_))
        ));
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        // SHA-256 of the empty input, the canonical test vector.
        let dir = scratch_dir("empty");
        let path = dir.join("empty.bin");
        std::fs::write(&path, b"").unwrap();
        assert_eq!(
            sha256_hex(&path).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_accepts_an_intact_artifact() {
        let (dir, manifest) = fixture("ok", b"a genuine shard bundle", |_| {});
        let verified = manifest
            .verify(&dir, &cell("r3gr"), &graph("sydney-2026-09-01"))
            .expect("intact artifact verifies");
        assert_eq!(verified.path, dir.join("r3gr.shard.rt"));
        assert_eq!(verified.artifact.nodes, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_a_tampered_bundle() {
        // The manifest is computed over the pristine bytes; then one byte of
        // the file on disk is flipped so the recomputed hash disagrees.
        let dir = scratch_dir("tamper");
        let path = dir.join("r3gr.shard.rt");
        let bundle = b"a genuine shard bundle".to_vec();
        std::fs::write(&path, &bundle).unwrap();
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "r3gr".to_owned(),
            Artifact {
                file: "r3gr.shard.rt".to_owned(),
                sha256: sha256_hex(&path).unwrap(),
                bytes: bundle.len() as u64,
                graph: graph("sydney-2026-09-01"),
                precision: SHARD_PRECISION,
                nodes: 10,
                edges: 20,
                built_at: "2026-09-14T00:00:00Z".to_owned(),
            },
        );
        let manifest = Manifest {
            version: 1,
            artifacts,
        };

        let mut tampered = bundle.clone();
        tampered[0] ^= 0xff;
        std::fs::write(&path, &tampered).unwrap();

        assert!(matches!(
            manifest.verify(&dir, &cell("r3gr"), &graph("sydney-2026-09-01")),
            Err(ArtifactError::ChecksumMismatch { cell }) if cell == "r3gr"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_wrong_graph() {
        let (dir, manifest) = fixture("graph", b"bundle", |_| {});
        assert!(matches!(
            manifest.verify(&dir, &cell("r3gr"), &graph("melbourne-2026-09-01")),
            Err(ArtifactError::GraphMismatch { cell, expected, got })
                if cell == "r3gr"
                    && expected == "melbourne-2026-09-01"
                    && got == "sydney-2026-09-01"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_a_missing_cell() {
        let (dir, manifest) = fixture("cell", b"bundle", |_| {});
        assert!(matches!(
            manifest.verify(&dir, &cell("r3gq"), &graph("sydney-2026-09-01")),
            Err(ArtifactError::MissingCell { cell }) if cell == "r3gq"
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_a_size_mismatch() {
        // Record a byte count larger than the file: the cheap size gate trips
        // before the hash is computed.
        let (dir, manifest) = fixture("size", b"bundle", |a| a.bytes += 1);
        assert!(matches!(
            manifest.verify(&dir, &cell("r3gr"), &graph("sydney-2026-09-01")),
            Err(ArtifactError::SizeMismatch { cell, expected, actual, .. })
                if cell == "r3gr" && expected == actual + 1
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_a_missing_file() {
        // A manifest that names a file the directory does not hold.
        let dir = scratch_dir("nofile");
        let mut artifacts = BTreeMap::new();
        artifacts.insert(
            "r3gr".to_owned(),
            Artifact {
                file: "r3gr.shard.rt".to_owned(),
                sha256: "0".repeat(64),
                bytes: 10,
                graph: graph("sydney-2026-09-01"),
                precision: SHARD_PRECISION,
                nodes: 1,
                edges: 1,
                built_at: "2026-09-14T00:00:00Z".to_owned(),
            },
        );
        let manifest = Manifest {
            version: 1,
            artifacts,
        };
        assert!(matches!(
            manifest.verify(&dir, &cell("r3gr"), &graph("sydney-2026-09-01")),
            Err(ArtifactError::MissingFile { path }) if path == dir.join("r3gr.shard.rt")
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn verify_rejects_wrong_precision() {
        let (dir, manifest) = fixture("prec", b"bundle", |a| a.precision = SHARD_PRECISION + 1);
        assert!(matches!(
            manifest.verify(&dir, &cell("r3gr"), &graph("sydney-2026-09-01")),
            Err(ArtifactError::PrecisionMismatch { cell, expected, actual })
                if cell == "r3gr" && expected == SHARD_PRECISION && actual == SHARD_PRECISION + 1
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_reads_a_written_manifest() {
        let dir = scratch_dir("load");
        std::fs::write(dir.join(MANIFEST_FILENAME), EXAMPLE).unwrap();
        let loaded = Manifest::load(&dir).expect("written manifest should load");
        assert_eq!(loaded, Manifest::parse(EXAMPLE).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_missing_manifest_is_reported() {
        let dir = scratch_dir("loadmissing");
        match Manifest::load(&dir) {
            Err(ArtifactError::MissingManifest { dir: reported }) => assert_eq!(reported, dir),
            other => panic!("expected MissingManifest, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn merge_is_newest_wins_per_cell() {
        fn artifact(built_at: &str, nodes: u64) -> Artifact {
            Artifact {
                file: "x.shard.rt".to_owned(),
                sha256: "0".repeat(64),
                bytes: 1,
                graph: graph("g"),
                precision: SHARD_PRECISION,
                nodes,
                edges: 0,
                built_at: built_at.to_owned(),
            }
        }

        let mut base = Manifest {
            version: 1,
            artifacts: BTreeMap::from([
                ("keep".to_owned(), artifact("2026-09-14T00:00:00Z", 1)),
                ("older".to_owned(), artifact("2026-09-10T00:00:00Z", 1)),
                ("newer".to_owned(), artifact("2026-09-20T00:00:00Z", 1)),
            ]),
        };
        let incoming = Manifest {
            version: 2,
            artifacts: BTreeMap::from([
                // A cell only the incoming batch has: added.
                ("fresh".to_owned(), artifact("2026-09-15T00:00:00Z", 9)),
                // Incoming is newer than base: replaces.
                ("older".to_owned(), artifact("2026-09-19T00:00:00Z", 9)),
                // Incoming is older than base: base is kept.
                ("newer".to_owned(), artifact("2026-09-15T00:00:00Z", 9)),
            ]),
        };

        base.merge(incoming);

        assert_eq!(base.version, 2, "higher version wins");
        assert_eq!(base.artifacts.len(), 4);
        assert_eq!(base.artifacts["keep"].nodes, 1, "untouched cell survives");
        assert_eq!(base.artifacts["fresh"].nodes, 9, "new cell added");
        assert_eq!(base.artifacts["older"].nodes, 9, "newer incoming replaces");
        assert_eq!(base.artifacts["newer"].nodes, 1, "older incoming ignored");
    }
}
