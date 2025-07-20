use geo::Rect;
use routers_tz_types::timezone::ResolvedTimezones;
use std::fmt::Debug;

// todo: docs
pub trait TimezoneResolver {
    type Error: Debug;

    fn search(&self, rect: &Rect) -> Result<ResolvedTimezones, Self::Error>;
}

#[cfg(test)]
mod tests {
    use crate::{RTreeStorage, TimezoneResolver};
    use geo::{BoundingRect, Point, point};
    use routers_tz_types::timezone::ResolvedTimezones;
    use std::fmt::Debug;
    use std::sync::OnceLock;
    use std::time::Instant;

    pub static RESOLVER: OnceLock<RTreeStorage> = OnceLock::new();

    #[ctor::ctor]
    fn init() {
        RESOLVER.get_or_init(|| {
            use crate::RTreeStorage;
            return RTreeStorage::new();
        });
    }

    // Helpers
    fn assert_singular(point: Point, expected: &str) {
        let possible_timezones = RESOLVER
            .get()
            .expect("timezones not initialized")
            .search(&point.bounding_rect())
            .expect("should have been resolved");

        assert!(
            matches!(possible_timezones, ResolvedTimezones::Singular(..)),
            "timezone was not singular, expected one resolved timezone"
        );

        assert_eq!(possible_timezones.tz(), expected);
    }

    #[test]
    fn locate_sydney() {
        let now = Instant::now();

        assert_singular(point! { x: 151.208211, y: -33.871075 }, "Australia/Sydney");

        let elapsed = now.elapsed();
        println!("[Search] Elapsed: {:.2?}", elapsed);
    }

    #[test]
    fn locate_broken_hill() {
        assert_singular(
            point! { x: 141.350077, y: -31.912325},
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
