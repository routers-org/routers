use geo::{Contains, MultiPolygon, Point, Polygon};

use crate::interface::{ResolvedTimezones, Timezone, TimezoneResolver};

struct BasicTimezone {
    timezone: Timezone,
    geometry: MultiPolygon,
}

pub struct BasicStorage {
    polygons: Vec<BasicTimezone>,
}

impl BasicStorage {
    pub fn new() -> Self {
        BasicStorage { polygons: vec![] }
    }
}

impl TimezoneResolver for BasicStorage {
    type Error = ();

    fn area(&self, _area: &Polygon) -> Result<ResolvedTimezones, Self::Error> {
        todo!()
    }

    fn point(&self, point: &Point) -> Result<ResolvedTimezones, Self::Error> {
        let mut resolved: Vec<Timezone> = vec![];

        for BasicTimezone { timezone, geometry } in &self.polygons {
            if geometry.contains(point) {
                resolved.push(*timezone)
            }
        }

        match resolved[..] {
            [] => Err(()),
            [one] => Ok(ResolvedTimezones::Singular(one)),
            _ => Ok(ResolvedTimezones::Many(resolved)),
        }
    }
}
