use chrono::{DateTime, Utc};
use geo::Point;
use routers::candidate::RoutedPath;
use routers::matcher::{Continuation, Trip};
use routers_network::{Entry, Metadata};
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use routers_transition::candidate::RoutedPath;
use serde::{Deserialize, Serialize};

use crate::store::Storable;

#[derive(Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct MatchContext<E: Entry> {
    pub vehicle_id: String,

    /// How the matcher should proceed, as reconciled by the orchestrator:
    /// [`Resume`](Continuation::Resume) carries the trellis from the prior
    /// solve plus the points it has not seen; [`Restart`](Continuation::Restart)
    /// means no prior solve stands (first point, or a diverged history) and
    /// the window is matched from scratch. The orchestrator can trim and
    /// compare but never generate a layer — pushing points stays with the
    /// matcher.
    pub continuation: Continuation<E>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct MatchResult<E: Entry, M: Metadata> {
    pub path: RoutedPath<E, M>,
    pub vehicle_id: String,
    pub trip: Trip<E>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    pub trip_id: String,
    pub vehicle_id: String,

    pub provider: String,

    /// When the observation was made. Serialized as microseconds since the
    /// Unix epoch on the wire.
    #[serde(with = "chrono::serde::ts_microseconds")]
    pub timestamp: DateTime<Utc>,

    pub point: Point,
}

impl Payload {
    pub fn as_event(&self) -> RawEvent {
        RawEvent {
            vehicle_id: self.vehicle_id.clone(),
            point: self.point,
            timestamp: self.timestamp,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RawEvent {
    pub vehicle_id: String,
    pub point: Point,

    /// When the observation was made. Serialized as microseconds since the
    /// Unix epoch on the wire.
    #[serde(with = "chrono::serde::ts_microseconds")]
    pub timestamp: DateTime<Utc>,
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
