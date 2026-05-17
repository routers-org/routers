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

impl Default for BasicStorage {
    fn default() -> Self {
        BasicStorage {
            backend: routers_tz_build::basic::storage(),
        }
    }
}

impl TimezoneResolver for BasicStorage {
    type Error = ();

    fn search(&self, rect: &Rect) -> Result<Vec<TimeZone>, Self::Error> {
        let timezones = self
            .backend
            .geometries
            .iter()
            .enumerate()
            .filter_map(|(index, TimeZoneGeometry(geometry))| {
                if geometry.contains(rect) {
                    Some(TimeZone::new(self.backend.names[index].tz()))
                } else {
                    None
                }
            })
            .collect::<Vec<TimeZone>>();

        match timezones[..] {
            [] => Err(()),
            _ => Ok(timezones),
        }
    }
}
