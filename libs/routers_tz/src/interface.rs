use core::fmt::Debug;
use geo::{BoundingRect, Polygon, Rect};
use routers_tz_types::TimeZone;

pub trait TimezoneResolver {
    type Error: Debug + From<()>;

    fn search(&self, rect: &Rect) -> Result<Vec<TimeZone>, Self::Error>;

    fn search_polygon(&self, polygon: &Polygon) -> Result<Vec<TimeZone>, Self::Error> {
        polygon
            .bounding_rect()
            .ok_or(Self::Error::from(()))
            .and_then(|rect| self.search(&rect))
    }
}

#[cfg(test)]
#[cfg(any(feature = "rtree", feature = "basic", feature = "s2cell"))]
mod tests {
    use crate::TimezoneResolver;

    use geo::{BoundingRect, Point, point};
    use std::sync::OnceLock;

    #[cfg(feature = "rtree")]
    pub static RESOLVER: OnceLock<crate::RTreeStorage> = OnceLock::new();

    #[cfg(feature = "basic")]
    pub static RESOLVER: OnceLock<crate::BasicStorage> = OnceLock::new();

    #[cfg(feature = "s2cell")]
    pub static RESOLVER: OnceLock<crate::S2CellStorage> = OnceLock::new();

    #[ctor::ctor]
    fn init() {
        #[cfg(feature = "rtree")]
        RESOLVER.get_or_init(|| crate::RTreeStorage::default());

        #[cfg(feature = "basic")]
        RESOLVER.get_or_init(|| crate::BasicStorage::default());

        #[cfg(feature = "s2cell")]
        RESOLVER.get_or_init(|| crate::S2CellStorage::default());
    }

    // Helpers
    pub fn assert_singular(point: Point, expected: &str) {
        let possible_timezones = RESOLVER
            .get()
            .expect("timezones not initialized")
            .search(&point.bounding_rect())
            .expect("should have been resolved");

        assert!(
            possible_timezones.len() == 1,
            "expected exactly one timezone"
        );

        let tz = possible_timezones
            .first()
            .expect("must have at least one timezone");

        assert_eq!(tz.name(), expected);
    }

    #[test]
    fn locate_sydney() {
        assert_singular(point! { x: 151.208211, y: -33.871075 }, "Australia/Sydney");
    }

    // Split from America/Ojinaga in tzdata 2022g.
    #[test]
    fn locate_ciudad_juarez() {
        assert_singular(point! { x: -106.45, y: 31.45 }, "America/Ciudad_Juarez")
    }

    #[test]
    fn locate_chicago() {
        assert_singular(point! { x: -87.64, y: 41.86350 }, "America/Chicago")
    }

    #[test]
    fn locate_zurich() {
        assert_singular(
            point! { x: 8.540560425944761, y: 47.373334621336284 },
            "Europe/Zurich",
        )
    }

    // Zones with polygons on both sides of the antimeridian.
    #[test]
    fn locate_across_the_antimeridian() {
        assert_singular(point! { x: 174.7633, y: -36.8485 }, "Pacific/Auckland");
        assert_singular(point! { x: 174.7762, y: -41.2865 }, "Pacific/Auckland");
        assert_singular(point! { x: 172.6362, y: -43.5321 }, "Pacific/Auckland");
        assert_singular(point! { x: 178.4419, y: -18.1416 }, "Pacific/Fiji");
        assert_singular(point! { x: 177.5, y: 64.73 }, "Asia/Anadyr");
        assert_singular(point! { x: -176.65, y: 51.88 }, "America/Adak");
    }

    // Each pair sits a few kilometres either side of a zone border.
    #[test]
    fn locate_either_side_of_a_border() {
        assert_singular(point! { x: -106.3778, y: 31.8072 }, "America/Denver");
        assert_singular(point! { x: -106.4245, y: 31.6904 }, "America/Ciudad_Juarez");
        assert_singular(point! { x: -83.07, y: 42.36 }, "America/Detroit");
        assert_singular(point! { x: -83.0, y: 42.28 }, "America/Toronto");
        assert_singular(point! { x: 146.92, y: -36.05 }, "Australia/Sydney");
        assert_singular(point! { x: 146.8878, y: -36.1218 }, "Australia/Melbourne");
        assert_singular(point! { x: 6.1432, y: 46.2044 }, "Europe/Zurich");
        assert_singular(point! { x: 7.8134, y: 48.5737 }, "Europe/Berlin");
    }
}
