use std::fmt::Debug;

use geo::{Contains, Rect};
use routers_tz_types::{
    TimeZone, storage::basic::BasicStorageBackend, timezone::internal::TimeZoneGeometry,
};

use crate::TimezoneResolver;

pub struct BasicStorage {
    backend: &'static BasicStorageBackend,
}

impl Debug for BasicStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BasicStorage")
    }
}

impl BasicStorage {
    pub fn new() -> BasicStorage {
        BasicStorage {
            backend: routers_tz_build::basic::storage(),
        }
    }
}

impl TimezoneResolver for BasicStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<TimeZone, Self::Error> {
        for (index, TimeZoneGeometry(geometry)) in self.backend.geometries.iter().enumerate() {
            if geometry.contains(rect) {
                return Ok(TimeZone::new(self.backend.names[index].tz()));
            }
        }

        Err(())
    }
}
