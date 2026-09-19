//! Versioned graph-artifact manifests shared by the shard generator and runtime loaders.
//!
//! A manifest is the single authority for mapping a canonical geohash cell to
//! its immutable `.shard.rt` bundle. Parsing rejects unsafe paths and malformed
//! checksums before a loader touches the filesystem.

use core::str::FromStr;
extern crate alloc;

use alloc::collections::BTreeMap;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::Geohash;

/// The only supported graph-artifact manifest format.
pub const MANIFEST_VERSION: u32 = 1;
/// The manifest file written beside shard bundles.
pub const MANIFEST_FILENAME: &str = "manifest.json";

/// A versioned, cell-keyed index of graph artifacts.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "RawManifest")]
pub struct Manifest {
    /// The schema version, currently [`MANIFEST_VERSION`].
    pub version: u32,
    /// Artifacts keyed by their canonical geohash cell.
    pub artifacts: BTreeMap<String, Artifact>,
}

/// Metadata for one immutable shard bundle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    /// The canonical `<cell>.shard.rt` filename, relative to the manifest.
    pub file: String,
    /// Lowercase hexadecimal SHA-256 digest of the bundle.
    pub sha256: String,
    /// Bundle size in bytes.
    pub bytes: u64,
    /// The token-safe graph snapshot version used to generate this bundle.
    pub graph: String,
    /// The geohash precision used to cut this bundle.
    pub precision: u8,
    /// Node count for provenance and capacity planning.
    pub nodes: u64,
    /// Edge count for provenance and capacity planning.
    pub edges: u64,
    /// RFC 3339 UTC generation timestamp.
    pub built_at: String,
}

/// A bundle whose manifest entry, regular-file status, size, and checksum were verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifiedArtifact {
    /// Resolved path of the checked bundle.
    pub path: PathBuf,
    /// The checked metadata.
    pub artifact: Artifact,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: u32,
    artifacts: BTreeMap<String, Artifact>,
}

/// Errors raised while parsing, locating, or verifying a graph artifact.
#[derive(Debug, Error)]
pub enum ArtifactError {
    /// No manifest exists at the expected location.
    #[error("no {MANIFEST_FILENAME} in {dir:?}")]
    MissingManifest { dir: PathBuf },
    /// Manifest JSON was syntactically invalid or did not match the schema.
    #[error("failed to parse manifest JSON: {0}")]
    Parse(#[from] serde_json::Error),
    /// The manifest schema version is unsupported.
    #[error("manifest version {actual} is unsupported; expected {expected}")]
    UnsupportedVersion { expected: u32, actual: u32 },
    /// A manifest key was not a canonical geohash cell.
    #[error("manifest has a non-canonical cell key {cell:?}")]
    InvalidCell { cell: String },
    /// An entry did not name exactly its canonical bundle filename.
    #[error(
        "artifact for cell {cell:?} names unsafe or non-canonical file {file:?}; expected {expected:?}"
    )]
    UnsafeFileName {
        cell: String,
        file: String,
        expected: String,
    },
    /// A digest was not exactly lowercase hexadecimal SHA-256.
    #[error("artifact for cell {cell:?} has invalid sha256 {sha256:?}")]
    InvalidChecksum { cell: String, sha256: String },
    /// A graph version was not safe for the message-topology token it later occupies.
    #[error("artifact for cell {cell:?} has invalid graph token {graph:?}")]
    InvalidGraph { cell: String, graph: String },
    /// No artifact was listed for a requested cell.
    #[error("manifest has no artifact for cell {cell:?}")]
    MissingCell { cell: String },
    /// The requested graph differs from the artifact graph.
    #[error("cell {cell:?} was built for graph {got:?}, expected {expected:?}")]
    GraphMismatch {
        cell: String,
        expected: String,
        got: String,
    },
    /// The requested geohash precision differs from the artifact precision.
    #[error("artifact {cell:?} has precision {actual}, expected {expected}")]
    PrecisionMismatch {
        cell: String,
        expected: u8,
        actual: u8,
    },
    /// The expected bundle was missing.
    #[error("artifact file {path:?} does not exist")]
    MissingFile { path: PathBuf },
    /// The bundle is not a regular file or is a symbolic link.
    #[error("artifact file {path:?} is not a regular non-symlink file")]
    UnsafeFile { path: PathBuf },
    /// The size differs from the manifest before hashing.
    #[error("artifact {cell:?} at {path:?} is {actual} bytes, manifest says {expected}")]
    SizeMismatch {
        cell: String,
        path: PathBuf,
        expected: u64,
        actual: u64,
    },
    /// The streamed digest differs from the manifest.
    #[error("artifact {cell:?} checksum does not match the manifest")]
    ChecksumMismatch { cell: String },
    /// Filesystem I/O failed.
    #[error("i/o error on {path:?}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl TryFrom<RawManifest> for Manifest {
    type Error = ArtifactError;

    fn try_from(raw: RawManifest) -> Result<Self, Self::Error> {
        if raw.version != MANIFEST_VERSION {
            return Err(ArtifactError::UnsupportedVersion {
                expected: MANIFEST_VERSION,
                actual: raw.version,
            });
        }

        for (cell, artifact) in &raw.artifacts {
            validate_artifact(cell, artifact)?;
        }

        Ok(Self {
            version: raw.version,
            artifacts: raw.artifacts,
        })
    }
}

impl Manifest {
    /// Construct the current-format manifest from generator-produced entries.
    #[must_use]
    pub fn new(artifacts: BTreeMap<String, Artifact>) -> Self {
        Self {
            version: MANIFEST_VERSION,
            artifacts,
        }
    }

    /// Parse and validate a manifest at the data boundary.
    pub fn parse(json: &str) -> Result<Self, ArtifactError> {
        let raw: RawManifest = serde_json::from_str(json)?;
        raw.try_into()
    }

    /// Read and validate `dir/manifest.json`.
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

    /// Replace manifest entries with new generator output and keep one entry per cell.
    pub fn merge(&mut self, other: Self) {
        self.artifacts.extend(other.artifacts);
    }

    /// Verify `cell` for one expected graph and precision before loading it.
    pub fn verify(
        &self,
        dir: impl AsRef<Path>,
        cell: &Geohash,
        expected_graph: &str,
        expected_precision: u8,
    ) -> Result<VerifiedArtifact, ArtifactError> {
        let dir = dir.as_ref();
        let key = cell.to_string();
        let artifact = self
            .artifacts
            .get(&key)
            .ok_or_else(|| ArtifactError::MissingCell { cell: key.clone() })?;

        if artifact.graph != expected_graph {
            return Err(ArtifactError::GraphMismatch {
                cell: key,
                expected: expected_graph.to_owned(),
                got: artifact.graph.clone(),
            });
        }
        if artifact.precision != expected_precision {
            return Err(ArtifactError::PrecisionMismatch {
                cell: key,
                expected: expected_precision,
                actual: artifact.precision,
            });
        }

        let path = dir.join(&artifact.file);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Err(ArtifactError::MissingFile { path });
            }
            Err(source) => return Err(ArtifactError::Io { path, source }),
        };
        if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
            return Err(ArtifactError::UnsafeFile { path });
        }
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
        if digest != artifact.sha256 {
            return Err(ArtifactError::ChecksumMismatch { cell: key });
        }
        Ok(VerifiedArtifact {
            path,
            artifact: artifact.clone(),
        })
    }
}

fn validate_artifact(cell: &str, artifact: &Artifact) -> Result<(), ArtifactError> {
    let parsed = Geohash::from_str(cell).map_err(|_| ArtifactError::InvalidCell {
        cell: cell.to_owned(),
    })?;
    if parsed.to_string() != cell {
        return Err(ArtifactError::InvalidCell {
            cell: cell.to_owned(),
        });
    }

    let expected = format!("{cell}.shard.rt");
    let path = Path::new(&artifact.file);
    let single_normal_component = matches!(path.components().next(), Some(Component::Normal(_)))
        && path.components().count() == 1;
    if !single_normal_component || artifact.file != expected {
        return Err(ArtifactError::UnsafeFileName {
            cell: cell.to_owned(),
            file: artifact.file.clone(),
            expected,
        });
    }

    if artifact.sha256.len() != 64
        || !artifact
            .sha256
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte <= b'f'))
    {
        return Err(ArtifactError::InvalidChecksum {
            cell: cell.to_owned(),
            sha256: artifact.sha256.clone(),
        });
    }
    if !token_safe(&artifact.graph) {
        return Err(ArtifactError::InvalidGraph {
            cell: cell.to_owned(),
            graph: artifact.graph.clone(),
        });
    }
    Ok(())
}

/// Stream the lowercase SHA-256 digest of a file in 1 MiB chunks.
pub fn sha256_hex(path: impl AsRef<Path>) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex_lower(&hasher.finalize()))
}

/// Whether a value is safe as a NATS subject token.
#[must_use]
pub fn token_safe(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[usize::from(byte >> 4)] as char);
        output.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact() -> Artifact {
        Artifact {
            file: "r3gq.shard.rt".to_owned(),
            sha256: "0".repeat(64),
            bytes: 1,
            graph: "test-graph".to_owned(),
            precision: 4,
            nodes: 0,
            edges: 0,
            built_at: "2026-09-14T00:00:00Z".to_owned(),
        }
    }

    #[test]
    fn manifest_rejects_unsafe_paths_and_weak_checksums() {
        let mut manifest = Manifest::new(BTreeMap::from([("r3gq".to_owned(), artifact())]));
        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        assert!(Manifest::parse(&json).is_ok());

        manifest.artifacts.get_mut("r3gq").expect("entry").file = "../elsewhere".to_owned();
        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        assert!(matches!(
            Manifest::parse(&json),
            Err(ArtifactError::UnsafeFileName { .. })
        ));

        let mut manifest = Manifest::new(BTreeMap::from([("r3gq".to_owned(), artifact())]));
        manifest.artifacts.get_mut("r3gq").expect("entry").sha256 = "A".repeat(64);
        let json = serde_json::to_string(&manifest).expect("serialize manifest");
        assert!(matches!(
            Manifest::parse(&json),
            Err(ArtifactError::InvalidChecksum { .. })
        ));
    }
}
