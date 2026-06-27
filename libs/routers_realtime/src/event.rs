use geo::{Coord, Point};
use routers_shard::{Geohash, GeohashStrategy, ShardId, ShardingStrategy};
use serde::{Deserialize, Serialize};

use crate::store::Storable;

#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    pub trip_id: String,
    pub vehicle_id: String,

    pub provider: String,
    pub event_ms: u64,

    pub point: Coord,
}

impl Storable for Payload {
    type ShardId = Geohash;
    type Key = String;

    fn shard_id(&self) -> Self::ShardId {
        GeohashStrategy::with_precision(5).locate(Point::new(self.point.x, self.point.y))
    }

    fn key(&self) -> Self::Key {
        self.vehicle_id.clone()
    }
}
