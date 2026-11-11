use jiff::Timestamp;
use jiff::tz::TimeZoneOffsetInfo;

pub mod internal {
    use super::TimeZone;
    use core::ops::Deref;
    use geo::{BoundingRect, MultiPolygon, Rect};
    use serde::{Deserialize, Serialize};

    /// An internal representation of a timezone used by the build
    /// steps to construct a timezone storage mechanism.
    #[derive(Debug)]
    pub struct TimezoneBuild {
        pub name: TimeZoneName,
        pub geometry: TimeZoneGeometry,
    }

    #[repr(transparent)]
    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct TimeZoneGeometry(pub MultiPolygon);

    impl TimeZoneGeometry {
        pub fn bbox(&self) -> Option<Rect> {
            self.0.bounding_rect()
        }
    }

    #[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
    #[repr(transparent)]
    pub struct TimeZoneName(String);

    impl TimeZoneName {
        pub fn new(name: String) -> Self {
            TimeZoneName(name)
        }
    }

    impl Deref for TimeZoneName {
        type Target = str;

        fn deref(&self) -> &Self::Target {
            self.0.as_str()
        }
    }

    impl TimeZoneName {
        /// Obtains the timezone information for a given IANA timezone name.
        ///
        /// # Panic Safety
        /// Function will panic if the tz does not exist.
        /// This should not happen as construction of this object must only exist
        /// as a function of the inverted operation in timezone creation.
        ///
        /// If this is a concern, use `try_tz`
        pub fn tz(&self) -> TimeZone {
            self.try_tz().expect("TimeZoneName not found in tz")
        }

        /// Tries to obtain the timezone information
        /// for a given timezone IANA name.
        pub fn try_tz(&self) -> Option<TimeZone> {
            TimeZone::get(self)
        }
    }

    impl PartialEq<&str> for &TimeZoneName {
        fn eq(&self, other: &&str) -> bool {
            self.0 == *other
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimeZone(jiff::tz::TimeZone);

impl TimeZone {
    /// The zone with this IANA name, from the bundled tz database.
    pub fn get(name: &str) -> Option<Self> {
        jiff::tz::TimeZone::get(name).ok().map(TimeZone)
    }

    pub fn name(&self) -> &str {
        self.0.iana_name().unwrap_or_default()
    }

    /// The offset, DST flag and abbreviation this zone observes at `instant`.
    pub fn at(&self, instant: Timestamp) -> TimeZoneOffsetInfo<'_> {
        self.0.to_offset_info(instant)
    }
}

#[cfg(test)]
mod tests {
    use super::TimeZone;
    use jiff::Timestamp;
    use jiff::tz::Offset;

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).expect("zone is in the bundled database")
    }

    fn instant(rfc3339: &str) -> Timestamp {
        rfc3339.parse().expect("valid timestamp")
    }

    #[test]
    fn offset_follows_daylight_saving() {
        let sydney = zone("Australia/Sydney");
        let summer = sydney.at(instant("2026-01-15T00:00:00Z"));
        let winter = sydney.at(instant("2026-07-15T00:00:00Z"));

        assert_eq!(summer.offset(), Offset::constant(11));
        assert!(summer.dst().is_dst());
        assert_eq!(summer.abbreviation(), "AEDT");
        assert_eq!(winter.offset(), Offset::constant(10));
        assert!(!winter.dst().is_dst());
    }

    #[test]
    fn offset_keeps_fractional_hours() {
        let at = instant("2026-01-15T00:00:00Z");

        assert_eq!(zone("Asia/Kolkata").at(at).offset().seconds(), 19_800);
        assert_eq!(zone("America/St_Johns").at(at).offset().seconds(), -12_600);
    }
}
