use geo::Coord;
use serde::{Deserialize, Serialize};

#[derive(Debug, Deserialize)]
pub struct CsvReplayEvent {
    #[serde(rename = "TripID")]
    pub trip_id: String,
    #[serde(rename = "VehicleID")]
    pub vehicle_id: String,
    #[serde(rename = "Provider")]
    pub provider: String,
    #[serde(rename = "EventTime")]
    pub event_time: String, // e.g., "2026-03-31 22:08:26 UTC"
    #[serde(rename = "Latitude")]
    pub latitude: f64,
    #[serde(rename = "Longitude")]
    pub longitude: f64,
    #[serde(rename = "PointGeom")]
    _point_geom: String, // Ignored in the final payload, but parsed from CSV
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    pub trip_id: String,
    pub vehicle_id: String,

    pub provider: String,
    pub event_time: String,

    pub point: Coord,
}
