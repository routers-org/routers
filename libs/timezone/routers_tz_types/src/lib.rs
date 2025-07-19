use bincode::{Decode, Encode};
use geo::MultiPolygon;
use serde::{Deserialize, Serialize};

// todo: docs
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
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
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Timezone {
    pub iana: IANATimezoneName,
}

// todo: docs
pub enum ResolvedTimezones {
    Singular(Timezone),
    Many(Vec<Timezone>),
}

impl ResolvedTimezones {
    pub fn tz(&self) -> &Timezone {
        use ResolvedTimezones::*;

        match self {
            Singular(tz) => tz,
            Many(tzs) => tzs.first().unwrap(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BasicTimezone {
    pub timezone: Timezone,
    pub geometry: MultiPolygon<f64>,
}

#[derive(Encode, Decode, Serialize, Deserialize, Debug, Clone)]
pub struct BasicStorageBackend {
    #[bincode(with_serde)]
    pub polygons: Vec<BasicTimezone>,
}
