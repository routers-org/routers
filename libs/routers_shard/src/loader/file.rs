//! Filesystem-backed [`Fetcher`] for native targets.

use std::path::PathBuf;

use thiserror::Error;

use super::fetcher::Fetcher;

#[derive(Debug, Clone)]
pub struct FileFetcher {
    base_dir: PathBuf,
}

impl FileFetcher {
    /// Creates a fetcher with a known base-directory to search from.
    pub fn new(base_dir: impl Into<PathBuf>) -> Self {
        Self {
            base_dir: base_dir.into(),
        }
    }
}

#[derive(Error, Debug)]
pub enum FileFetchError {
    #[error("io error: {0}")]
    Io(std::io::Error),
}

impl Fetcher for FileFetcher {
    type Error = FileFetchError;

    async fn fetch(&self, key: &str) -> Result<Vec<u8>, Self::Error> {
        let path = self.base_dir.join(key);
        std::fs::read(&path).map_err(FileFetchError::Io)
    }
}
