use bincode::{Decode, Encode};
use geo::{BoundingRect, MultiPolygon, Rect};
use serde::{Deserialize, Serialize};

// todo: docs
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Timezone {
    pub iana: IANATimezoneName,
    pub geometry: MultiPolygon<f64>,
}

impl Timezone {
    pub fn bounding_box(&self) -> Option<Rect> {
        self.geometry.bounding_rect()
    }
}

// todo: docs
#[derive(Encode, Decode, Serialize, Deserialize, Clone, Debug, PartialEq)]
#[repr(transparent)]
pub struct IANATimezoneName(pub String);

impl Default for IANATimezoneName {
    fn default() -> IANATimezoneName {
        IANATimezoneName("".to_string())
    }
}

impl PartialEq<&str> for IANATimezoneName {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

// todo: docs
pub enum ResolvedTimezones {
    Singular(IANATimezoneName),
    Many(Vec<IANATimezoneName>),
}

impl ResolvedTimezones {
    pub fn tz(self) -> IANATimezoneName {
        use ResolvedTimezones::*;

        match self {
            Singular(tz) => tz,
            Many(tzs) => tzs.first().unwrap().clone(),
        }
    }
}
