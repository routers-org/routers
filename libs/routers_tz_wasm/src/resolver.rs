//! The lookup core, free of component types so it builds and tests on the
//! host. The bindings in `bindings.rs` map the WIT interface onto this.

use alloc::vec::Vec;

use geo::{BoundingRect, Point, Polygon, Rect};
use routers_tz::{S2CellStorage, TimeZone, TimezoneResolver};

/// The timezones at a point; empty where no zone is mapped.
pub fn at_point(point: Point) -> Vec<TimeZone> {
    in_rect(&point.bounding_rect())
}

/// The timezones at the centre of a rectangle.
pub fn in_rect(rect: &Rect) -> Vec<TimeZone> {
    S2CellStorage::default().search(rect).unwrap_or_default()
}

/// The timezones at the centre of a polygon's bounds; empty for an empty ring.
pub fn in_polygon(polygon: &Polygon) -> Vec<TimeZone> {
    S2CellStorage::default()
        .search_polygon(polygon)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{LineString, coord, point};

    fn names(zones: Vec<TimeZone>) -> Vec<&'static str> {
        zones.iter().map(|tz| tz.name()).collect()
    }

    #[test]
    fn point_resolves_its_zone() {
        let sydney = point! { x: 151.208211, y: -33.871075 };
        assert_eq!(names(at_point(sydney)), ["Australia/Sydney"]);
    }

    #[test]
    fn rect_resolves_its_centre() {
        let chicago = Rect::new(coord! { x: -87.7, y: 41.8 }, coord! { x: -87.6, y: 41.9 });
        assert_eq!(names(in_rect(&chicago)), ["America/Chicago"]);
    }

    #[test]
    fn polygon_resolves_its_centre() {
        let zurich = Polygon::new(
            LineString::from(vec![(8.5, 47.3), (8.6, 47.3), (8.6, 47.4), (8.5, 47.4)]),
            vec![],
        );
        assert_eq!(names(in_polygon(&zurich)), ["Europe/Zurich"]);
    }

    #[test]
    fn empty_polygon_resolves_nothing() {
        let empty = Polygon::new(LineString::new(vec![]), vec![]);
        assert!(in_polygon(&empty).is_empty());
    }
}
