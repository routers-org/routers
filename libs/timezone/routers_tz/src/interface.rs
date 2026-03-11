use geo::Rect;
use std::fmt::Debug;

// todo: docs
pub trait TimezoneResolver {
    type Error: Debug;

    fn search(&self, rect: &Rect) -> Result<routers_tz_types::TimeZone, Self::Error>;
}

#[cfg(test)]
mod tests {
    use crate::{RTreeStorage, TimezoneResolver};
    use geo::{BoundingRect, Point, point};
    use std::sync::OnceLock;
    use std::time::Instant;
    use time_tz::TimeZone;

    pub static RESOLVER: OnceLock<RTreeStorage> = OnceLock::new();

    #[ctor::ctor]
    fn init() {
        RESOLVER.get_or_init(|| {
            use crate::RTreeStorage;
            return RTreeStorage::default();
        });
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
