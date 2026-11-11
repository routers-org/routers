use jiff::Timestamp;
use jiff::tz::TimeZoneOffsetInfo;
use time::{OffsetDateTime, UtcOffset};

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

    /// The offset from UTC this zone observes at `instant`.
    pub fn offset_at(&self, instant: &OffsetDateTime) -> UtcOffset {
        let seconds = self.info_at(instant).offset().seconds();
        UtcOffset::from_whole_seconds(seconds).unwrap_or(UtcOffset::UTC)
    }

    /// Whether this zone observes daylight saving time at `instant`.
    pub fn is_dst_at(&self, instant: &OffsetDateTime) -> bool {
        self.info_at(instant).dst().is_dst()
    }

    fn info_at(&self, instant: &OffsetDateTime) -> TimeZoneOffsetInfo<'_> {
        let nanos = instant.unix_timestamp_nanos();
        // jiff's range is slightly narrower than `time`'s, so clamp at the ends.
        let clamped = if nanos < 0 {
            Timestamp::MIN
        } else {
            Timestamp::MAX
        };
        self.0
            .to_offset_info(Timestamp::from_nanosecond(nanos).unwrap_or(clamped))
    }
}

#[cfg(test)]
mod tests {
    use super::TimeZone;
    use time::macros::{datetime, offset};

    fn zone(name: &str) -> TimeZone {
        TimeZone::get(name).expect("zone is in the bundled database")
    }

    #[test]
    fn offset_follows_daylight_saving() {
        let sydney = zone("Australia/Sydney");
        let (summer, winter) = (
            datetime!(2026-01-15 0:00 UTC),
            datetime!(2026-07-15 0:00 UTC),
        );

        assert_eq!(sydney.offset_at(&summer), offset!(+11));
        assert!(sydney.is_dst_at(&summer));
        assert_eq!(sydney.offset_at(&winter), offset!(+10));
        assert!(!sydney.is_dst_at(&winter));
    }

    #[test]
    fn offset_keeps_fractional_hours() {
        let at = datetime!(2026-01-15 0:00 UTC);

        assert_eq!(zone("Asia/Kolkata").offset_at(&at), offset!(+5:30));
        assert_eq!(zone("America/St_Johns").offset_at(&at), offset!(-3:30));
    }
}
