use geo::{Contains, Point, Polygon};
use routers_tz_types::{BasicStorageBackend, BasicTimezone, ResolvedTimezones, Timezone};

use crate::TimezoneResolver;

pub struct BasicStorage {
    backend: &'static BasicStorageBackend,
}

impl BasicStorage {
    pub fn new() -> BasicStorage {
        BasicStorage {
            backend: routers_tz_build::data::get_timezone_storage(),
        }
    }
}

impl TimezoneResolver for BasicStorage {
    type Error = ();

    fn point(&self, point: &Point) -> Result<ResolvedTimezones, Self::Error> {
        let mut resolved: Vec<Timezone> = vec![];

        for BasicTimezone { timezone, geometry } in &self.backend.polygons {
            if geometry.contains(point) {
                resolved.push(timezone.clone())
            }
        }

        match resolved.as_slice() {
            [] => Err(()),
            [one] => Ok(ResolvedTimezones::Singular(one.clone())),
            _ => Ok(ResolvedTimezones::Many(resolved)),
        }
    }

    fn area(&self, _area: &Polygon) -> Result<ResolvedTimezones, Self::Error> {
        todo!()
    }
}
