use geo::Coord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    pub trip_id: String,
    pub vehicle_id: String,

    pub provider: String,
    pub event_ms: u64,

    pub point: Coord,
}
