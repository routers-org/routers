use crate::timezone::internal::{TimeZoneGeometry, TimeZoneName};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub struct BasicStorageBackend {
    pub geometries: Vec<TimeZoneGeometry>,
    pub names: Vec<TimeZoneName>,
}
