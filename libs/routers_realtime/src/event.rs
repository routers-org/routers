use chrono::{DateTime, Utc};
use geo::Point;
use routers_network::{Entry, Metadata};
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use geo::LineString;
use routers_network::Network;
use routers_transition::LayerId;
use routers_transition::candidate::{CollapsedPath, RoutedPath};
use routers_transition::matcher::{Continuation, Trip};
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

/// A vehicle's matched trace as per-observation interpolated segments, split at
/// the coalescence boundary into the part that can never change and the part
/// still subject to revision (see `COALESCENCE.md` in `routers_trellis`).
///
/// Each segment is the road geometry arriving at one matched observation;
/// concatenating `stable` then `tentative` in order rebuilds the full
/// interpolated trace, and a consumer stitches its bounded view from them.
///
/// `stable` segments are final: no later position can change them, so a
/// consumer already holding them may drop the repeat. The matcher re-sends
/// them because it keeps no per-consumer state — collapsing that to a
/// once-only emission is the register tier's job (dedupe by the committed
/// watermark). `tentative` is the volatile tail from the coalescence anchor to
/// the current position, and supersedes any earlier tentative tail wholesale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceUpdate {
    pub vehicle_id: String,
    pub stable: Vec<LineString>,
    pub tentative: Vec<LineString>,
}

impl TraceUpdate {
    /// Build the split from a solved match: the per-observation interpolated
    /// segments up to and including the [`stable_upto`](Trip::stable_upto)
    /// layer become `stable`, the rest `tentative`. `stable_upto` is `None`
    /// when nothing has converged, leaving the whole trace tentative.
    pub fn from_match<E: Entry, M: Metadata>(
        vehicle_id: String,
        solution: &CollapsedPath<E>,
        map: &impl Network<E, M>,
        stable_upto: Option<LayerId>,
    ) -> Self {
        let mut stable = solution.interpolated_segments(map);
        let committed = stable_upto
            .map_or(0, |layer| layer.index() + 1)
            .min(stable.len());
        let tentative = stable.split_off(committed);

        Self {
            vehicle_id,
            stable,
            tentative,
        }
    }
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
