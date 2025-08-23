use std::ops::Deref;
use time_tz::Tz;

pub mod internal {
    use bincode::{Decode, Encode};
    use geo::{BoundingRect, MultiPolygon, Rect};
    use std::ops::Deref;
    use time_tz::Tz;

    /// An internal representation of a timezone used by the build
    /// steps to construct a timezone storage mechanism.
    #[derive(Debug)]
    pub struct TimezoneBuild {
        pub tz: &'static Tz,
        pub name: TimeZoneName,
        pub geometry: TimeZoneGeometry,
    }

    #[repr(transparent)]
    #[derive(Encode, Decode, Clone, Debug)]
    pub struct TimeZoneGeometry(#[bincode(with_serde)] pub MultiPolygon);

    impl TimeZoneGeometry {
        pub fn bbox(&self) -> Option<Rect> {
            self.0.bounding_rect()
        }
    }

    #[derive(Encode, Decode, Clone, Debug, PartialEq)]
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
        pub fn tz(&self) -> &'static Tz {
            time_tz::timezones::get_by_name(self).expect("TimeZoneName not found in tz")
        }

        /// Tries to obtain the timezone information
        /// for a given timezone IANA name.
        pub fn try_tz(&self) -> Option<&'static Tz> {
            time_tz::timezones::get_by_name(self)
        }
    }

    impl PartialEq<&str> for &TimeZoneName {
        fn eq(&self, other: &&str) -> bool {
            self.0 == *other
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct TimeZone(&'static Tz);

impl TimeZone {
    pub fn new(tz: &'static Tz) -> Self {
        TimeZone(tz)
    }
}

impl Deref for TimeZone {
    type Target = Tz;
    fn deref(&self) -> &Self::Target {
        self.0
    }
}
