//! A Valkey-backed [`Sink`]: the served view as hashes.
//!
//! [`ValkeySink`] is the production backend: one vehicle's history is a set of
//! Valkey hashes (keys hash-tagged on the vehicle so a future Cluster keeps them
//! on one slot), and applying an output loads the affected segment, folds it in
//! with the shared [`merge`], and writes back only what changed. An apply is not
//! atomic against readers, which is safe because the merge is idempotent and a
//! reader resolves layers by revision itself.

use alloc::collections::BTreeMap;

use redis::aio::MultiplexedConnection;
use routers_network::Entry;
use serde::de::DeserializeOwned;
use thiserror::Error;
use url::Url;

use crate::event::VehicleId;
use crate::materializer::sink::{Applied, SegmentState, Sink, StoredLayer, merge, target_segment};
use crate::partition::{fnv1a, mix};
use crate::protocol::ids::{Revision, SegmentId};
use crate::protocol::output::{CommittedOutput, OutputKind};

/// Why a Valkey apply failed.
#[derive(Debug, Error)]
pub enum ValkeyError {
    /// The Valkey command failed.
    #[error("valkey: {0}")]
    Valkey(#[from] redis::RedisError),
    /// A stored layer could not be (de)serialised.
    #[error("serialisation: {0}")]
    Serialisation(#[from] postcard::Error),
    /// No primaries were supplied.
    #[error("no valkey endpoints supplied")]
    NoEndpoints,
}

/// Which primary owns a vehicle: rendezvous placement over the fleet, kept as a
/// pure function so the mapping stays stable across a fleet change.
#[derive(Clone)]
struct Placement {
    /// One hash per endpoint URL, so identity is the URL, not the list position.
    seeds: Vec<u64>,
}

impl Placement {
    fn new(urls: &[Url]) -> Self {
        Self {
            seeds: urls
                .iter()
                .map(|url| fnv1a(url.as_str().as_bytes()))
                .collect(),
        }
    }

    /// Rendezvous (highest-random-weight) placement: growing the fleet remaps
    /// about `1/N` of vehicles.
    fn index_for(&self, key: &str) -> usize {
        let hash = fnv1a(key.as_bytes());
        self.seeds
            .iter()
            .enumerate()
            // The seed breaks ties, so the winner never depends on list order.
            .max_by_key(|(_, seed)| (mix(hash ^ **seed), **seed))
            .map(|(index, _)| index)
            .expect("fleet is non-empty, checked in ValkeySink::connect")
    }
}

/// The HASH holding one segment's layers.
fn segment_key(vehicle: VehicleId, segment: SegmentId) -> String {
    format!("matched:{{{}}}:{}", vehicle.0, segment.0)
}

/// The per-vehicle metadata HASH.
fn meta_key(vehicle: VehicleId) -> String {
    format!("matched:{{{}}}:meta", vehicle.0)
}

/// The metadata field holding a segment's finality watermark.
fn finalized_field(segment: SegmentId) -> String {
    format!("finalized_through:{}", segment.0)
}

/// A fleet of independent Valkey primaries backing the materialised view,
/// addressed by vehicle. Cloning shares the underlying sockets.
#[derive(Clone)]
pub struct ValkeySink {
    conns: Vec<MultiplexedConnection>,
    placement: Placement,
}

impl ValkeySink {
    /// Connect to every primary in `urls`. The set is unordered, but every
    /// process that touches the view must be handed the same set, or a vehicle's
    /// history splits across primaries.
    pub async fn connect(urls: &[Url]) -> Result<Self, ValkeyError> {
        if urls.is_empty() {
            return Err(ValkeyError::NoEndpoints);
        }
        let mut conns = Vec::with_capacity(urls.len());
        for url in urls {
            let conn = redis::Client::open(url.clone())?
                .get_multiplexed_async_connection()
                .await?;
            conns.push(conn);
        }
        Ok(Self {
            conns,
            placement: Placement::new(urls),
        })
    }

    /// The connection for a vehicle, so all its keys land on one primary.
    fn connection(&self, vehicle: VehicleId) -> MultiplexedConnection {
        let node = self.placement.index_for(&vehicle.0.to_string());
        self.conns[node].clone()
    }
}

/// One segment's layers, loaded from its HASH.
async fn load_layers<E: Entry + DeserializeOwned>(
    conn: &mut MultiplexedConnection,
    vehicle: VehicleId,
    segment: SegmentId,
) -> Result<BTreeMap<i64, StoredLayer<E>>, ValkeyError> {
    let raw: std::collections::HashMap<String, Vec<u8>> = redis::cmd("HGETALL")
        .arg(segment_key(vehicle, segment))
        .query_async(conn)
        .await?;

    let mut layers = BTreeMap::new();
    for (field, bytes) in raw {
        // Skip fields that are not timestamps rather than fail.
        let Ok(timestamp) = field.parse::<i64>() else {
            continue;
        };
        layers.insert(timestamp, postcard::from_bytes(&bytes)?);
    }
    Ok(layers)
}

/// The vehicle's metadata HASH, read as plain strings.
async fn read_meta(
    conn: &mut MultiplexedConnection,
    vehicle: VehicleId,
) -> Result<std::collections::HashMap<String, String>, ValkeyError> {
    Ok(redis::cmd("HGETALL")
        .arg(meta_key(vehicle))
        .query_async(conn)
        .await?)
}

/// The highest revision among a segment's stored layers, for observability.
fn highest_revision<E: Entry>(layers: &BTreeMap<i64, StoredLayer<E>>) -> Option<Revision> {
    layers
        .values()
        .map(|stored| stored.revision)
        .max_by_key(|revision| revision.0)
}

impl<E: Entry + DeserializeOwned> Sink<E> for ValkeySink {
    type Error = ValkeyError;

    async fn apply(&self, output: &CommittedOutput<E>) -> Result<Applied, ValkeyError> {
        let vehicle = output.vehicle_id;
        let segment = target_segment(output);
        let mut conn = self.connection(vehicle);

        let layers = load_layers::<E>(&mut conn, vehicle, segment).await?;
        let meta = read_meta(&mut conn, vehicle).await?;
        let had_current = meta.contains_key("current_segment");
        let mut state = SegmentState {
            finalized_through: meta
                .get(&finalized_field(segment))
                .and_then(|value| value.parse::<i64>().ok()),
            last_revision: highest_revision(&layers),
            layers,
        };

        // Snapshot the stored revisions so the write-back touches only changes.
        let before: BTreeMap<i64, Revision> = state
            .layers
            .iter()
            .map(|(timestamp, stored)| (*timestamp, stored.revision))
            .collect();
        let before_finalized = state.finalized_through;

        let applied = merge(&mut state, output);

        let layer_key = segment_key(vehicle, segment);
        let meta_key = meta_key(vehicle);
        let mut pipe = redis::pipe();
        let mut dirty = false;

        for (timestamp, stored) in &state.layers {
            if before.get(timestamp) != Some(&stored.revision) {
                let bytes = postcard::to_allocvec(stored)?;
                pipe.cmd("HSET")
                    .arg(&layer_key)
                    .arg(*timestamp)
                    .arg(bytes)
                    .ignore();
                dirty = true;
            }
        }
        for timestamp in before.keys() {
            if !state.layers.contains_key(timestamp) {
                pipe.cmd("HDEL").arg(&layer_key).arg(*timestamp).ignore();
                dirty = true;
            }
        }
        if state.finalized_through != before_finalized
            && let Some(through) = state.finalized_through
        {
            pipe.cmd("HSET")
                .arg(&meta_key)
                .arg(finalized_field(segment))
                .arg(through)
                .ignore();
            dirty = true;
        }
        // A reset moves the current segment; a vehicle's first output sets it.
        if matches!(output.kind, OutputKind::Reset { .. }) || !had_current {
            pipe.cmd("HSET")
                .arg(&meta_key)
                .arg("current_segment")
                .arg(segment.0)
                .ignore();
            dirty = true;
        }

        if dirty {
            pipe.query_async::<()>(&mut conn).await?;
        }

        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_hash_tagged_on_the_vehicle() {
        assert_eq!(segment_key(VehicleId(42), SegmentId(7)), "matched:{42}:7");
        assert_eq!(meta_key(VehicleId(42)), "matched:{42}:meta");
        assert_eq!(finalized_field(SegmentId(7)), "finalized_through:7");
    }

    #[test]
    fn placement_is_deterministic_and_order_independent() {
        let urls: Vec<Url> = (0..8)
            .map(|i| Url::parse(&format!("redis://valkey-{i:02}:6379")).unwrap())
            .collect();
        let mut reversed = urls.clone();
        reversed.reverse();

        let direct = Placement::new(&urls);
        let flipped = Placement::new(&reversed);

        for vehicle in 0..1000u64 {
            let key = vehicle.to_string();
            assert_eq!(
                urls[direct.index_for(&key)],
                reversed[flipped.index_for(&key)]
            );
        }
    }
}
