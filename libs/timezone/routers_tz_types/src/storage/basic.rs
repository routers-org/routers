use crate::timezone::internal::{TimeZoneGeometry, TimeZoneName};
use bincode::{Decode, Encode};

#[derive(Encode, Decode, Debug)]
pub struct BasicStorageBackend {
    pub geometries: Vec<TimeZoneGeometry>,
    pub names: Vec<TimeZoneName>,
}
