use serde::Serialize;
use std::fs;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

use crate::BoxError;

/// Directory (relative to the crate root) where pre-baked backend data is
/// committed. These files are shipped in the published crate so consumers
/// don't need the source geojson to build.
pub const PREBUILT_DIR: &str = "data/prebuilt";

/// Describes one storage backend: its module name under
/// `routers_tz_types::storage::{module}`.
pub struct Backend<'a> {
    pub module: &'a str,
}

impl Backend<'_> {
    pub fn data_path(&self) -> PathBuf {
        PathBuf::from(PREBUILT_DIR).join(format!("{}_timezone_data.postcard.bin", self.module))
    }

    /// Serialise `value` into the backend's pre-built data file.
    pub fn emit(&self, value: impl Serialize) -> Result<(), BoxError> {
        let bytes = postcard::to_allocvec(&value)
            .map_err(|e| format!("failed to serialise {}: {e}", self.module))?;
        let path = self.data_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        File::create(&path)?.write_all(&bytes)?;
        eprintln!("[{}] wrote {}", self.module, path.display());
        Ok(())
    }
}
