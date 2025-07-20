use geo::{Contains, Rect};
use routers_tz_types::storage::basic::BasicStorageBackend;
use routers_tz_types::timezone::{IANATimezoneName, ResolvedTimezones, Timezone};

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

    fn search(&self, rect: &Rect) -> Result<ResolvedTimezones, Self::Error> {
        let mut resolved: Vec<IANATimezoneName> = vec![];

        for Timezone { iana, geometry } in &self.backend.polygons {
            if geometry.contains(rect) {
                resolved.push(iana.clone())
            }
        }

        match resolved.as_slice() {
            [] => Err(()),
            [one] => Ok(ResolvedTimezones::Singular(one.clone())),
            _ => Ok(ResolvedTimezones::Many(resolved)),
        }
    }
}
