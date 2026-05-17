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
    use time_tz::TimeZone;

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

        assert_eq!(possible_timezones.name(), expected);
    }

    #[test]
    fn locate_sydney() {
        assert_singular(point! { x: 151.208211, y: -33.871075 }, "Australia/Sydney");
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
}
