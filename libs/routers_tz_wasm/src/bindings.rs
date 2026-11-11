//! Component bindings: generate the Rust types/traits from `wit/world.wit` and
//! map the exported `resolver` interface onto the [`resolver`](crate::resolver)
//! core. Compiled only for wasm.

use alloc::string::ToString;
use alloc::vec::Vec;

use geo::{Coord, LineString, Point, Polygon, Rect};
use routers_tz::{TimeZone, Timestamp};

use crate::resolver;

wit_bindgen::generate!({
    world: "timezones",
});

use exports::routers::timezone::resolver as wit;

type Resolved<T> = Result<T, wit::Error>;

struct Component;

impl wit::Guest for Component {
    fn get_from_point(
        coordinate: wit::Coordinate,
        timestamp: Option<wit::Timestamp>,
    ) -> Resolved<Vec<wit::Timezone>> {
        let instant = instant_of(timestamp)?;
        Ok(all(resolver::at_point(point_of(&coordinate)), instant))
    }

    fn batch_get_from_points(
        coordinates: Vec<wit::Coordinate>,
        timestamp: Option<wit::Timestamp>,
    ) -> Resolved<Vec<Option<wit::Timezone>>> {
        let instant = instant_of(timestamp)?;
        Ok(coordinates
            .iter()
            .map(|c| first(resolver::at_point(point_of(c)), instant))
            .collect())
    }

    fn get_from_bounding_box(
        bounding_box: wit::BoundingBox,
        timestamp: Option<wit::Timestamp>,
    ) -> Resolved<Vec<wit::Timezone>> {
        let instant = instant_of(timestamp)?;
        Ok(all(resolver::in_rect(&rect_of(&bounding_box)), instant))
    }

    fn batch_get_from_bounding_box(
        bounding_boxes: Vec<wit::BoundingBox>,
        timestamp: Option<wit::Timestamp>,
    ) -> Resolved<Vec<Option<wit::Timezone>>> {
        let instant = instant_of(timestamp)?;
        Ok(bounding_boxes
            .iter()
            .map(|bb| first(resolver::in_rect(&rect_of(bb)), instant))
            .collect())
    }

    fn get_from_polygon(
        polygon: wit::Polygon,
        timestamp: Option<wit::Timestamp>,
    ) -> Resolved<Vec<wit::Timezone>> {
        let instant = instant_of(timestamp)?;
        Ok(all(resolver::in_polygon(&polygon_of(&polygon)), instant))
    }

    fn batch_get_from_polygon(
        polygons: Vec<wit::Polygon>,
        timestamp: Option<wit::Timestamp>,
    ) -> Resolved<Vec<Option<wit::Timezone>>> {
        let instant = instant_of(timestamp)?;
        Ok(polygons
            .iter()
            .map(|p| first(resolver::in_polygon(&polygon_of(p)), instant))
            .collect())
    }
}

/// The requested instant, or now when the caller leaves it unset.
fn instant_of(timestamp: Option<wit::Timestamp>) -> Resolved<Timestamp> {
    // Not `Timestamp::now()`: its panic path formats a jiff error, which
    // links ~230 KiB of formatting code into the component.
    match timestamp {
        Some(ts) => Timestamp::new(ts.seconds, ts.nanos),
        None => Timestamp::try_from(std::time::SystemTime::now()),
    }
    .map_err(|_| wit::Error::InvalidTimestamp)
}

fn coord_of(c: &wit::Coordinate) -> Coord {
    Coord {
        x: c.longitude,
        y: c.latitude,
    }
}

fn point_of(c: &wit::Coordinate) -> Point {
    Point(coord_of(c))
}

fn rect_of(bb: &wit::BoundingBox) -> Rect {
    Rect::new(coord_of(&bb.top_left), coord_of(&bb.bottom_right))
}

fn polygon_of(p: &wit::Polygon) -> Polygon {
    let ring: Vec<Coord> = p.coordinates.iter().map(coord_of).collect();
    Polygon::new(LineString::from(ring), Vec::new())
}

fn timezone_of(tz: TimeZone, instant: Timestamp) -> wit::Timezone {
    let observed = tz.at(instant);
    let offset = observed.offset().seconds();

    wit::Timezone {
        utc_offset: wit::UtcOffset {
            seconds: offset % 60,
            minutes: offset / 60 % 60,
            hours: offset / 3600,
        },
        iana_code: tz.name().to_string(),
        is_dst: observed.dst().is_dst(),
    }
}

fn all(zones: Vec<TimeZone>, instant: Timestamp) -> Vec<wit::Timezone> {
    zones
        .into_iter()
        .map(|tz| timezone_of(tz, instant))
        .collect()
}

fn first(zones: Vec<TimeZone>, instant: Timestamp) -> Option<wit::Timezone> {
    zones.into_iter().next().map(|tz| timezone_of(tz, instant))
}

export!(Component);
