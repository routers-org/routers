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

    /// Touch the geojson to rebuild after changing a backend's builder.
    pub fn is_fresh(&self) -> bool {
        let path = self.data_path();
        let modified = |path: &std::path::Path| fs::metadata(path).and_then(|m| m.modified()).ok();
        let fresh = modified(&path)
            .zip(modified(crate::geojson::geojson_path().as_ref()))
            .is_some_and(|(built, source)| built >= source);
        if fresh {
            eprintln!(
                "[{}] pre-built {} is fresh, skipping recompute",
                self.module,
                path.display()
            );
        }
        fresh
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
