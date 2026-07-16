use geo::Point;
use routers::candidate::RoutedPath;
use routers_network::{Entry, Metadata};
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use serde::{Deserialize, Serialize};

use crate::store::Storable;

#[derive(Debug, Serialize, Deserialize)]
pub struct MatchContext {
    pub history: Vec<Point>,
    pub vehicle_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MatchResult<E: Entry, M: Metadata> {
    pub path: RoutedPath<E, M>,
    pub vehicle_id: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    pub trip_id: String,
    pub vehicle_id: String,

    pub provider: String,
    pub event_ms: u64,

    pub point: Point,
}

impl Payload {
    pub fn as_event(&self) -> RawEvent {
        RawEvent {
            vehicle_id: self.vehicle_id.clone(),
            point: self.point,
            event_ms: self.event_ms,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawEvent {
    pub vehicle_id: String,
    pub point: Point,
    pub event_ms: u64,
}

impl Storable for RawEvent {
    type ShardId = Geohash;
    type Key = String;

    fn shard_id(&self) -> Self::ShardId {
        GeohashStrategy::with_precision(4).locate(self.point)
    }

    fn key(&self) -> Self::Key {
        self.vehicle_id.clone()
    }
}
