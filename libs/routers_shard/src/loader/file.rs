//! Filesystem-backed [`ShardFetcher`] for native targets.
//!
//! Useful for local development and for the build pipeline's
//! "round-trip" tests. The browser uses [`WebShardFetcher`](super::WebShardFetcher)
//! instead — this file is excluded from the wasm32 build entirely.

use std::path::PathBuf;

use super::fetcher::ShardFetcher;

#[derive(Debug, Clone)]
pub struct FileShardFetcher {
    base_dir: PathBuf,
}

impl FileShardFetcher {
    /// Look up keys relative to `base_dir`. Treats the key as a path
    /// suffix, so a key like `"sydney/cbd.shard.rt"` reads
    /// `<base_dir>/sydney/cbd.shard.rt`.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Debug)]
pub enum FileFetchError {
    Io(std::io::Error),
}

impl core::fmt::Display for FileFetchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FileFetchError::Io(e) => write!(f, "I/O error: {e}"),
        }
    }
}

impl ShardFetcher for FileShardFetcher {
    type Error = FileFetchError;

    async fn fetch(&self, key: &str) -> Result<Vec<u8>, Self::Error> {
        let path = self.base_dir.join(key);
        std::fs::read(&path).map_err(FileFetchError::Io)
    }
}
