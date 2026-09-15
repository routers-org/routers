use chrono::{DateTime, Utc};
use geo::Point;
use routers_network::{Edge, Entry, Network};
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};
use routers_transition::candidate::CollapsedPath;
use routers_transition::matcher::Origin;
use serde::{Deserialize, Serialize};

use buffa::Message;
use schema::proto::routers::realtime::v1 as proto;

use crate::bus::Wire;

/// Declare a compact wire identifier. The upstream string ids are stepped
/// down to these at the ingest boundary (see the replay binary), so the
/// strings never cross the wire again.
macro_rules! wire_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub u64);

        impl From<u64> for $name {
            fn from(value: u64) -> Self {
                Self(value)
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

wire_id! {
    /// Identifies one vehicle across its events, history, and matches:
    /// the FNV-1a 64-bit hash of the upstream string id.
    VehicleId
}

/// One layer of matched history: the observation's identity (its timestamp),
/// where it matched, and the road geometry driven since the previous layer.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct MatchedLayer<E: Entry> {
    /// When the observation was made (microseconds since the Unix epoch).
    /// With the vehicle, this addresses the layer fleet-wide: overlapping
    /// emissions merge on it.
    pub timestamp: i64,

    /// The directed network edge the observation matched onto.
    pub edge: Edge<E>,

    /// The observation, snapped onto the edge.
    pub position: Point,

    /// Interpolated road geometry between the previous layer's position and
    /// this one. Empty on a diff's first layer: its inbound hop was emitted
    /// while both endpoints were still in the trip.
    pub path: Vec<Point>,
}

/// Everything a solve could still change, emitted whole: one layer per trip
/// observation since the last convergence cut. Consumers merge layers by
/// (vehicle, timestamp) and resolve competing solves by revision — highest
/// wins, equal is a duplicate — so re-emission is convergence, not conflict.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct MatchedDiff<E: Entry> {
    /// Orders competing solves for a vehicle. Becomes the stream sequence of
    /// the triggering ingest message once durable ingest lands; 0 until then.
    pub revision: u64,

    /// Set when a resume was rejected and the solve restarted from raw
    /// history — the emitted region may rewrite more than usual.
    pub downgraded: bool,

    pub layers: Vec<MatchedLayer<E>>,
}

impl<E: Entry> MatchedDiff<E> {
    /// Lift a solved trip into its emission: one layer per observation, hops
    /// resolved to geometry against the (matcher-local) network.
    pub fn new<N: Network<Entry = E>>(
        solution: &CollapsedPath<'_, E>,
        origins: &[Origin],
        map: &N,
        revision: u64,
    ) -> Self {
        let layers = solution
            .route
            .iter()
            .zip(origins)
            .enumerate()
            .filter_map(|(index, (chosen, origin))| {
                let candidate = solution.candidates.candidate(chosen)?;

                // Hop `i - 1` carries the roads driven into layer `i`; the
                // endpoints' own positions live on their layers.
                let path = match index.checked_sub(1) {
                    Some(hop) => solution.hop_geometry(hop, map),
                    None => Vec::new(),
                };

                Some(MatchedLayer {
                    timestamp: origin.timestamp,
                    edge: candidate.edge,
                    position: candidate.position,
                    path,
                })
            })
            .collect();

        Self {
            revision,
            downgraded: false,
            layers,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Payload {
    pub vehicle_id: VehicleId,

    /// When the observation was made. Serialized as microseconds since the
    /// Unix epoch on the wire.
    #[serde(with = "chrono::serde::ts_microseconds")]
    pub timestamp: DateTime<Utc>,

    pub point: Point,
}

/// The ingest surface crosses the bus as protobuf
/// (`routers.realtime.v1.Payload`), so producers in any language can
/// publish raw events against the schema.
impl Wire for Payload {
    fn encode(&self) -> anyhow::Result<Vec<u8>> {
        Ok(proto::Payload::from(self).encode_to_vec())
    }

    fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        Ok(Self::from(proto::Payload::decode_from_slice(bytes)?))
    }
}

impl From<&Payload> for proto::Payload {
    fn from(payload: &Payload) -> Self {
        proto::Payload {
            vehicle_id: payload.vehicle_id.0,
            timestamp: buffa::MessageField::some(
                buffa_types::google::protobuf::Timestamp::from_unix(
                    payload.timestamp.timestamp(),
                    payload.timestamp.timestamp_subsec_nanos() as i32,
                ),
            ),
            point: buffa::MessageField::some(schema::proto::routers::model::v1::Coordinate {
                longitude: payload.point.x(),
                latitude: payload.point.y(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
}

impl From<proto::Payload> for Payload {
    fn from(payload: proto::Payload) -> Self {
        let point = payload.point.into_option().unwrap_or_default();
        let timestamp = payload.timestamp.into_option().unwrap_or_default();

        Payload {
            vehicle_id: VehicleId(payload.vehicle_id),
            timestamp: DateTime::from_timestamp(timestamp.seconds, timestamp.nanos as u32)
                .unwrap_or_default(),
            point: Point::new(point.longitude, point.latitude),
        }
    }
}

/// The fleet's geographic shard precision. One source of truth: matcher
/// subjects, storage tags, and shard files must all agree on it.
pub const SHARD_PRECISION: u8 = 4;

/// The geographic shard an observation belongs to. Orchestrators route match
/// requests to `events.match.<shard_of(point)>`; matchers each serve the one
/// shard they loaded.
pub fn shard_of(point: Point) -> Geohash {
    GeohashStrategy::with_precision(SHARD_PRECISION).locate(point)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ingest surface must survive its protobuf round trip exactly:
    /// a foreign producer encoding `routers.realtime.v1.Payload` and this
    /// crate must agree on every field.
    #[test]
    fn payload_round_trips_over_the_wire() {
        let payload = Payload {
            // Above u32::MAX, so a truncation anywhere in the round trip fails.
            vehicle_id: VehicleId(0xdead_beef_cafe_f00d),
            timestamp: DateTime::from_timestamp_micros(1_775_000_000_123_456).unwrap(),
            point: Point::new(150.871294, -33.938879),
        };

        let bytes = payload.encode().expect("payload must encode");
        let decoded = Payload::decode(&bytes).expect("payload must decode");

        assert_eq!(decoded.vehicle_id, payload.vehicle_id);
        assert_eq!(decoded.timestamp, payload.timestamp);
        assert_eq!(decoded.point, payload.point);
    }
}
