use geo::{Point, Polygon};
use routers_tz_types::ResolvedTimezones;

// todo: docs
pub trait TimezoneResolver {
    type Error;

    fn point(&self, point: &Point) -> Result<ResolvedTimezones, Self::Error>;
    fn area(&self, area: &Polygon) -> Result<ResolvedTimezones, Self::Error>;
}

#[cfg(test)]
mod tests {
    use crate::{BasicStorage, TimezoneResolver};
    use geo::{Point, point};
    use routers_tz_types::ResolvedTimezones;

    // Helpers
    fn assert_singular(point: Point, expected: &str) {
        let resolver = BasicStorage::new();
        let possible_timezones = resolver.point(&point).expect("should have been resolved");

        assert!(
            matches!(possible_timezones, ResolvedTimezones::Singular(..)),
            "timezone was not singular, expected one resolved timezone"
        );

        assert_eq!(possible_timezones.tz().iana, expected);
    }

    #[test]
    fn locate_sydney() {
        assert_singular(point! { x: 151.208211, y: -33.871075 }, "Australia/Sydney");
    }

    #[test]
    fn locate_broken_hill() {
        assert_singular(
            point! { x:  141.350077, y: -31.912325},
            "Australia/Broken_Hill",
        )
    }

    #[test]
    fn locate_zurich() {
        assert_singular(
            point! { x: 8.540560425944761, y: 47.373334621336284 },
            "Europe/Zurich",
        )
    }
}
