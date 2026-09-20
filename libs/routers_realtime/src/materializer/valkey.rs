//! A Valkey-backed [`Sink`]: the served view as hashes.
//!
//! [`ValkeySink`] is the production backend: one vehicle's history is a set of
//! Valkey hashes (keys hash-tagged on the vehicle so a future Cluster keeps them
//! on one slot), and applying an output loads the affected segment, folds it in
//! with the shared [`merge`], and writes back only what changed. A per-vehicle
//! version guard rejects stale snapshots; one Lua script then applies every
//! layer, tombstone, and metadata mutation atomically.
//!
//! Every concurrent writer uses this versioned script; direct writes would not
//! advance `state_version` and therefore cannot participate in the CAS protocol.

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use redis::aio::MultiplexedConnection;
use routers_network::Entry;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::event::VehicleId;
use crate::materializer::sink::{Applied, SegmentState, Sink, StoredLayer, merge, target_segment};
use crate::partition::{fnv1a, mix};
use crate::protocol::ids::{Revision, SegmentId};
use crate::protocol::output::{CommittedOutput, OutputKind};
use crate::store::valkey::ValkeyEndpoint;

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
    /// Two primaries claimed the same stable rendezvous identity.
    #[error("duplicate valkey node id {0:?}")]
    DuplicateNodeId(String),
    /// A materialised vehicle's optimistic version was not a valid unsigned integer.
    #[error("invalid materialized view version {0:?}")]
    MalformedVersion(String),
    /// Concurrent writers changed a vehicle on every retry; redelivery may retry safely.
    #[error("materialized view changed during every compare-and-set retry")]
    CasExhausted,
    /// The materialised view version cannot advance further.
    #[error("materialized view version exhausted")]
    VersionExhausted,
}

/// The metadata field that serialises every mutation for one vehicle. It is
/// deliberately vehicle-wide, not segment-wide: `current_segment` is shared
/// by all segments and must not be computed from a stale snapshot.
const STATE_VERSION_FIELD: &str = "state_version";

/// Bound a hot writer's work before returning an error to the consumer. The
/// unacknowledged output is redelivered, so a conflict never loses an update.
const CAS_RETRIES: usize = 8;

/// Apply a precomputed write set if the vehicle has not changed since it was
/// read. `KEYS[1]` is the target segment hash and `KEYS[2]` is the vehicle
/// metadata hash; both carry the same `{vehicle}` hash tag. The first two args
/// are the expected and next vehicle versions. Every following four-tuple is
/// target (`L`ayer or `M`etadata), operation (`S`et or `D`elete), field, value.
const APPLY_LUA: &str = r#"
local count = tonumber(ARGV[3])
if not count or count < 0 or count ~= math.floor(count) then
  return redis.error_reply('invalid materializer mutation count')
end

-- A Lua script does not roll back commands that ran before an error. Validate
-- every possible failure from our generated arguments before the first write.
for _, key in ipairs(KEYS) do
  local kind = redis.call('TYPE', key).ok
  if kind ~= 'none' and kind ~= 'hash' then
    return redis.error_reply('materializer key is not a hash')
  end
end

local current = redis.call('HGET', KEYS[2], 'state_version') or '0'
if current ~= ARGV[1] then
  return 0
end

local arg = 4
for _ = 1, count do
  local target = ARGV[arg]
  local operation = ARGV[arg + 1]
  local field = ARGV[arg + 2]
  local value = ARGV[arg + 3]
  if target ~= 'L' and target ~= 'M' then
    return redis.error_reply('unknown materializer mutation target')
  end
  if operation ~= 'S' and operation ~= 'D' then
    return redis.error_reply('unknown materializer mutation')
  end
  if not field or not value then
    return redis.error_reply('incomplete materializer mutation')
  end
  arg = arg + 4
end

local mutation_arg = 4
for _ = 1, count do
  local key = ARGV[mutation_arg] == 'L' and KEYS[1] or KEYS[2]
  local operation = ARGV[mutation_arg + 1]
  local field = ARGV[mutation_arg + 2]
  local value = ARGV[mutation_arg + 3]
  if operation == 'S' then
    redis.call('HSET', key, field, value)
  else
    redis.call('HDEL', key, field)
  end
  mutation_arg = mutation_arg + 4
end

redis.call('HSET', KEYS[2], 'state_version', ARGV[2])
return 1
"#;

/// Which primary owns a vehicle: rendezvous placement over the fleet, kept as a
/// pure function so the mapping stays stable across a fleet change.
#[derive(Clone)]
struct Placement {
    /// One hash per stable node id, independent of URL and list position.
    seeds: Vec<u64>,
}

impl Placement {
    fn new(endpoints: &[ValkeyEndpoint]) -> Self {
        Self {
            seeds: endpoints
                .iter()
                .map(|endpoint| fnv1a(endpoint.id.as_str().as_bytes()))
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

fn validate_endpoints(endpoints: &[ValkeyEndpoint]) -> Result<(), ValkeyError> {
    if endpoints.is_empty() {
        return Err(ValkeyError::NoEndpoints);
    }
    let mut ids = std::collections::HashSet::with_capacity(endpoints.len());
    for endpoint in endpoints {
        if !ids.insert(endpoint.id.clone()) {
            return Err(ValkeyError::DuplicateNodeId(
                endpoint.id.as_str().to_owned(),
            ));
        }
    }
    Ok(())
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

/// The metadata field holding the newest output revision applied to a segment.
fn last_revision_field(segment: SegmentId) -> String {
    format!("last_revision:{}", segment.0)
}

/// A fleet of independent Valkey primaries backing the materialised view,
/// addressed by vehicle. Cloning shares the underlying sockets.
#[derive(Clone)]
pub struct ValkeySink {
    conns: Vec<MultiplexedConnection>,
    placement: Placement,
    apply_script: Arc<redis::Script>,
}

impl ValkeySink {
    /// Connect to every primary in `endpoints`. The set is unordered, but every
    /// process that touches the view must be handed the same set, or a vehicle's
    /// history splits across primaries. Each apply uses a vehicle-wide
    /// compare-and-set script, so concurrent materializers cannot overwrite a
    /// snapshot that another writer has already advanced.
    pub async fn connect(endpoints: &[ValkeyEndpoint]) -> Result<Self, ValkeyError> {
        validate_endpoints(endpoints)?;
        let mut conns = Vec::with_capacity(endpoints.len());
        for endpoint in endpoints {
            let conn = redis::Client::open(endpoint.url.connection_url())?
                .get_multiplexed_async_connection()
                .await?;
            conns.push(conn);
        }
        Ok(Self {
            conns,
            placement: Placement::new(endpoints),
            apply_script: Arc::new(redis::Script::new(APPLY_LUA)),
        })
    }

    /// The connection for a vehicle, so all its keys land on one primary.
    fn connection(&self, vehicle: VehicleId) -> MultiplexedConnection {
        let node = self.placement.index_for(&vehicle.0.to_string());
        self.conns[node].clone()
    }
}

/// One field mutation in either the segment or vehicle metadata hash.
enum HashMutation {
    Set { field: String, value: Vec<u8> },
    Delete { field: String },
}

/// The complete mutation produced by folding one output into one snapshot.
/// Sending it to Valkey through [`APPLY_LUA`] is all-or-nothing.
#[derive(Default)]
struct WritePlan {
    layers: Vec<HashMutation>,
    metadata: Vec<HashMutation>,
}

/// The two hashes that form one compare-and-set input snapshot.
struct StoredSnapshot<E: Entry> {
    meta: std::collections::HashMap<String, String>,
    layers: BTreeMap<i64, StoredLayer<E>>,
    retractions: BTreeMap<i64, Revision>,
}

/// Decoded contents of one segment HASH.
struct StoredSegment<E: Entry> {
    layers: BTreeMap<i64, StoredLayer<E>>,
    retractions: BTreeMap<i64, Revision>,
}

/// Metadata fields needed to merge one output and construct its CAS write.
fn snapshot_fields(segment: SegmentId) -> [String; 5] {
    [
        STATE_VERSION_FIELD.into(),
        "current_segment".into(),
        "current_revision".into(),
        finalized_field(segment),
        last_revision_field(segment),
    ]
}

/// Only layer-changing outputs need the potentially large segment HASH.
fn needs_segment_state<E: Entry>(kind: &OutputKind<E>) -> bool {
    matches!(
        kind,
        OutputKind::Matched { .. } | OutputKind::Retraction { .. }
    )
}

impl WritePlan {
    fn is_empty(&self) -> bool {
        self.layers.is_empty() && self.metadata.is_empty()
    }

    fn len(&self) -> usize {
        self.layers.len() + self.metadata.len()
    }
}

/// Read the versioned metadata before the segment hash. If another writer
/// mutates between those two reads it increments this version, causing the
/// subsequent compare-and-set to reject the mixed snapshot.
fn state_version(meta: &std::collections::HashMap<String, String>) -> Result<u64, ValkeyError> {
    match meta.get(STATE_VERSION_FIELD) {
        Some(version) => version
            .parse()
            .map_err(|_| ValkeyError::MalformedVersion(version.clone())),
        None => Ok(0),
    }
}

/// Atomically install `plan` only when `expected_version` still names the
/// snapshot it was derived from. Returns `false` on a benign CAS conflict.
async fn apply_plan(
    script: &redis::Script,
    conn: &mut MultiplexedConnection,
    layer_key: &str,
    metadata_key: &str,
    expected_version: u64,
    plan: &WritePlan,
) -> Result<bool, ValkeyError> {
    let next_version = expected_version
        .checked_add(1)
        .ok_or(ValkeyError::VersionExhausted)?;
    let mut invocation = script.prepare_invoke();
    invocation
        .key(layer_key)
        .key(metadata_key)
        .arg(expected_version)
        .arg(next_version)
        .arg(plan.len());

    for mutation in &plan.layers {
        match mutation {
            HashMutation::Set { field, value } => {
                invocation
                    .arg("L")
                    .arg("S")
                    .arg(field)
                    .arg(value.as_slice());
            }
            HashMutation::Delete { field } => {
                invocation.arg("L").arg("D").arg(field).arg("");
            }
        }
    }
    for mutation in &plan.metadata {
        match mutation {
            HashMutation::Set { field, value } => {
                invocation
                    .arg("M")
                    .arg("S")
                    .arg(field)
                    .arg(value.as_slice());
            }
            HashMutation::Delete { field } => {
                invocation.arg("M").arg("D").arg(field).arg("");
            }
        }
    }

    let applied: i64 = invocation.invoke_async(conn).await?;
    Ok(applied == 1)
}

/// Decode one segment HASH into live layers and retraction tombstones.
fn decode_layers<E: Entry + DeserializeOwned>(
    raw: std::collections::HashMap<String, Vec<u8>>,
) -> Result<StoredSegment<E>, ValkeyError> {
    let mut layers = BTreeMap::new();
    let mut retractions = BTreeMap::new();
    for (field, bytes) in raw {
        if let Some(timestamp) = field
            .strip_prefix("retracted:")
            .and_then(|value| value.parse::<i64>().ok())
        {
            retractions.insert(timestamp, postcard::from_bytes(&bytes)?);
            continue;
        }
        // Skip fields that are neither layers nor recognised tombstones.
        let Ok(timestamp) = field.parse::<i64>() else {
            continue;
        };
        layers.insert(timestamp, postcard::from_bytes(&bytes)?);
    }
    Ok(StoredSegment {
        layers,
        retractions,
    })
}

/// Load the versioned metadata before the segment HASH in one network round
/// trip. Redis executes the pipelined commands in order; if another writer
/// advances the version after the first read, the later CAS rejects this
/// snapshot exactly as it did when these were separate round trips.
async fn load_snapshot<E: Entry + DeserializeOwned>(
    conn: &mut MultiplexedConnection,
    vehicle: VehicleId,
    segment: SegmentId,
    include_segment: bool,
) -> Result<StoredSnapshot<E>, ValkeyError> {
    let fields = snapshot_fields(segment);
    let metadata_key = meta_key(vehicle);
    let (values, raw): (
        Vec<Option<String>>,
        std::collections::HashMap<String, Vec<u8>>,
    ) = if include_segment {
        let mut pipeline = redis::pipe();
        pipeline
            // This must remain first: the CAS version makes a later mixed
            // snapshot retry instead of overwriting concurrent state.
            .cmd("HMGET")
            .arg(&metadata_key)
            .arg(&fields)
            .cmd("HGETALL")
            .arg(segment_key(vehicle, segment));
        pipeline.query_async(conn).await?
    } else {
        let values = redis::cmd("HMGET")
            .arg(&metadata_key)
            .arg(&fields)
            .query_async(conn)
            .await?;
        (values, std::collections::HashMap::new())
    };
    let meta = fields
        .into_iter()
        .zip(values)
        .filter_map(|(field, value)| value.map(|value| (field, value)))
        .collect();
    let StoredSegment {
        layers,
        retractions,
    } = decode_layers(raw)?;
    Ok(StoredSnapshot {
        meta,
        layers,
        retractions,
    })
}

impl<E: Entry + DeserializeOwned> Sink<E> for ValkeySink {
    type Error = ValkeyError;

    async fn apply(&self, output: &CommittedOutput<E>) -> Result<Applied, ValkeyError> {
        let vehicle = output.vehicle_id;
        let segment = target_segment(output);
        let mut conn = self.connection(vehicle);
        let layer_key = segment_key(vehicle, segment);
        let meta_key = meta_key(vehicle);

        for _ in 0..CAS_RETRIES {
            // Read the version first; see `state_version` for why this order
            // makes a mixed read harmless rather than a lost update.
            let StoredSnapshot {
                meta,
                layers,
                retractions,
            } = load_snapshot::<E>(
                &mut conn,
                vehicle,
                segment,
                needs_segment_state(&output.kind),
            )
            .await?;
            let version = state_version(&meta)?;
            let had_current = meta.contains_key("current_segment");
            let current_segment = meta
                .get("current_segment")
                .and_then(|value| value.parse::<u64>().ok())
                .map(SegmentId);
            let current_revision = meta
                .get("current_revision")
                .and_then(|value| value.parse::<u64>().ok())
                .map(Revision);
            let mut state = SegmentState {
                finalized_through: meta
                    .get(&finalized_field(segment))
                    .and_then(|value| value.parse::<i64>().ok()),
                last_revision: meta
                    .get(&last_revision_field(segment))
                    .and_then(|value| value.parse::<u64>().ok())
                    .map(Revision),
                layers,
                retractions,
            };

            // Snapshot the stored revisions so the write-back touches only changes.
            let before: BTreeMap<i64, Revision> = state
                .layers
                .iter()
                .map(|(timestamp, stored)| (*timestamp, stored.revision))
                .collect();
            let before_finalized = state.finalized_through;
            let before_revision = state.last_revision;
            let before_retractions = state.retractions.clone();

            let applied = merge(&mut state, output);
            let mut plan = WritePlan::default();

            for (timestamp, stored) in &state.layers {
                if before.get(timestamp) != Some(&stored.revision) {
                    plan.layers.push(HashMutation::Set {
                        field: timestamp.to_string(),
                        value: postcard::to_allocvec(stored)?,
                    });
                }
            }
            for timestamp in before.keys() {
                if !state.layers.contains_key(timestamp) {
                    plan.layers.push(HashMutation::Delete {
                        field: timestamp.to_string(),
                    });
                }
            }
            for (timestamp, revision) in &state.retractions {
                if before_retractions.get(timestamp) != Some(revision) {
                    plan.layers.push(HashMutation::Set {
                        field: format!("retracted:{timestamp}"),
                        value: postcard::to_allocvec(revision)?,
                    });
                }
            }
            for timestamp in before_retractions.keys() {
                if !state.retractions.contains_key(timestamp) {
                    plan.layers.push(HashMutation::Delete {
                        field: format!("retracted:{timestamp}"),
                    });
                }
            }
            if state.finalized_through != before_finalized
                && let Some(through) = state.finalized_through
            {
                plan.metadata.push(HashMutation::Set {
                    field: finalized_field(segment),
                    value: through.to_string().into_bytes(),
                });
            }
            if state.last_revision != before_revision
                && let Some(revision) = state.last_revision
            {
                plan.metadata.push(HashMutation::Set {
                    field: last_revision_field(segment),
                    value: revision.0.to_string().into_bytes(),
                });
            }

            // A reset moves the current segment only when it is not stale.
            // Outputs in the current segment advance its revision even when
            // they carry no layers, so a late reset cannot roll the vehicle
            // backwards.
            let reset = matches!(output.kind, OutputKind::Reset { .. });
            let update_current = !had_current
                || (reset && current_revision.is_none_or(|current| output.revision.0 >= current.0))
                || (!reset
                    && current_segment == Some(segment)
                    && current_revision.is_none_or(|current| output.revision.0 > current.0));
            if update_current {
                plan.metadata.push(HashMutation::Set {
                    field: "current_segment".into(),
                    value: segment.0.to_string().into_bytes(),
                });
                plan.metadata.push(HashMutation::Set {
                    field: "current_revision".into(),
                    value: output.revision.0.to_string().into_bytes(),
                });
            }

            if plan.is_empty() {
                // This is a true no-op: `merge` leaves the state and its
                // monotonic last revision unchanged only when this output is
                // already dominated. A concurrent writer can only advance
                // those facts, so it cannot turn this duplicate into a write.
                return Ok(applied);
            }
            if apply_plan(
                &self.apply_script,
                &mut conn,
                &layer_key,
                &meta_key,
                version,
                &plan,
            )
            .await?
            {
                return Ok(applied);
            }
        }

        Err(ValkeyError::CasExhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::MatchedDiff;
    use crate::protocol::output::{ResetReason, TerminalReason};
    use routers_network::mock::MockEntryId;

    /// Minimal model of the script's version gate. The real mutation is Lua;
    /// this lets adversarial ordering remain an ordinary unit test.
    fn cas<T>(version: &mut u64, expected: u64, mutation: impl FnOnce() -> T) -> Option<T> {
        if *version != expected {
            return None;
        }
        let applied = mutation();
        *version = version
            .checked_add(1)
            .expect("test version does not overflow");
        Some(applied)
    }

    fn hash_tag(key: &str) -> Option<&str> {
        let (_, after_open) = key.split_once('{')?;
        let (tag, _) = after_open.split_once('}')?;
        (!tag.is_empty()).then_some(tag)
    }

    #[test]
    fn keys_are_hash_tagged_on_the_vehicle() {
        let layer = segment_key(VehicleId(42), SegmentId(7));
        let metadata = meta_key(VehicleId(42));
        assert_eq!(layer, "matched:{42}:7");
        assert_eq!(metadata, "matched:{42}:meta");
        assert_eq!(hash_tag(&layer), Some("42"));
        assert_eq!(hash_tag(&layer), hash_tag(&metadata));
        // APPLY_LUA writes only the supplied segment and metadata hashes, so
        // this shared tag also makes its multi-key mutation Cluster-safe.
        assert!(APPLY_LUA.contains("KEYS[1]"));
        assert!(APPLY_LUA.contains("KEYS[2]"));
        assert!(APPLY_LUA.contains("target ~= 'L' and target ~= 'M'"));
        assert!(APPLY_LUA.contains("unknown materializer mutation target"));
        assert_eq!(finalized_field(SegmentId(7)), "finalized_through:7");
        assert_eq!(last_revision_field(SegmentId(7)), "last_revision:7");
    }

    #[test]
    fn snapshot_fields_are_minimal_and_segment_scoped() {
        assert_eq!(
            snapshot_fields(SegmentId(7)),
            [
                "state_version",
                "current_segment",
                "current_revision",
                "finalized_through:7",
                "last_revision:7",
            ]
        );
    }

    #[test]
    fn only_layer_mutations_load_the_segment_hash() {
        let matched = OutputKind::<MockEntryId>::Matched {
            diff: MatchedDiff {
                revision: 1,
                downgraded: false,
                layers: Vec::new(),
            },
            finalized_through: None,
        };
        let retraction = OutputKind::<MockEntryId>::Retraction {
            timestamps: vec![1],
        };
        let terminal = OutputKind::<MockEntryId>::Terminal {
            reason: TerminalReason::Unanchored,
            closes_segment: false,
        };
        let reset = OutputKind::<MockEntryId>::Reset {
            reason: ResetReason::Gap,
            new_segment: SegmentId(2),
        };

        assert!(needs_segment_state(&matched));
        assert!(needs_segment_state(&retraction));
        assert!(!needs_segment_state(&terminal));
        assert!(!needs_segment_state(&reset));
    }

    #[test]
    fn endpoints_require_unique_stable_ids() {
        let duplicate = [
            "primary=redis://one:6379".parse().unwrap(),
            "primary=redis://two:6379".parse().unwrap(),
        ];
        assert!(matches!(
            validate_endpoints(&duplicate),
            Err(ValkeyError::DuplicateNodeId(id)) if id == "primary"
        ));
    }

    #[test]
    fn stale_writer_cannot_apply_its_precomputed_mutations() {
        let mut version = 0;
        let mut written = Vec::new();

        assert_eq!(cas(&mut version, 0, || written.push("writer-a")), Some(()));
        // Writer B read the same version before A committed. Its plan must not
        // run, so it cannot overwrite A's layer/tombstone/metadata update.
        assert_eq!(
            cas(&mut version, 0, || written.push("writer-b-stale")),
            None
        );
        assert_eq!(written, ["writer-a"]);
        assert_eq!(version, 1);
    }

    #[test]
    fn conflicted_writer_rebases_before_its_next_compare_and_set() {
        let mut version = 0;
        let mut highest_layer_revision = 0;

        assert!(
            cas(&mut version, 0, || {
                highest_layer_revision = highest_layer_revision.max(8);
            })
            .is_some()
        );
        // A stale write cannot install revision 7 over revision 8.
        assert!(
            cas(&mut version, 0, || {
                highest_layer_revision = highest_layer_revision.max(7);
            })
            .is_none()
        );
        // After reloading the versioned snapshot, retrying the reducer's
        // revision rule preserves the winner rather than reviving stale data.
        assert!(
            cas(&mut version, 1, || {
                highest_layer_revision = highest_layer_revision.max(7);
            })
            .is_some()
        );

        assert_eq!(highest_layer_revision, 8);
        assert_eq!(version, 2);
    }

    #[test]
    fn empty_metadata_starts_at_zero_but_malformed_version_is_rejected() {
        let empty = std::collections::HashMap::new();
        assert_eq!(state_version(&empty).unwrap(), 0);

        let mut malformed = std::collections::HashMap::new();
        malformed.insert(STATE_VERSION_FIELD.into(), "not-a-number".into());
        assert!(matches!(
            state_version(&malformed),
            Err(ValkeyError::MalformedVersion(_))
        ));
    }

    #[test]
    fn apply_script_preflights_types_and_all_mutations_before_writing() {
        let type_check = APPLY_LUA
            .find("redis.call('TYPE', key).ok")
            .expect("script checks both key types");
        let tuple_check = APPLY_LUA
            .find("unknown materializer mutation target")
            .expect("script validates mutation targets");
        let mutation_loop = APPLY_LUA
            .find("local mutation_arg = 4")
            .expect("script has a post-validation mutation loop");
        let first_write = APPLY_LUA
            .find("redis.call('HSET', key, field, value)")
            .expect("script writes only after validation");

        assert!(type_check < tuple_check);
        assert!(tuple_check < mutation_loop);
        assert!(mutation_loop < first_write);
        assert!(APPLY_LUA.contains("materializer key is not a hash"));
        assert!(APPLY_LUA.contains("unknown materializer mutation"));
        assert!(APPLY_LUA.contains("incomplete materializer mutation"));
    }

    #[test]
    fn placement_is_deterministic_and_order_independent() {
        let urls: Vec<ValkeyEndpoint> = (0..8)
            .map(|i| {
                format!("node-{i:02}=redis://valkey-{i:02}:6379")
                    .parse()
                    .unwrap()
            })
            .collect();
        let mut reversed = urls.clone();
        reversed.reverse();

        let direct = Placement::new(&urls);
        let flipped = Placement::new(&reversed);

        for vehicle in 0..1000u64 {
            let key = vehicle.to_string();
            assert_eq!(
                urls[direct.index_for(&key)].id,
                reversed[flipped.index_for(&key)].id
            );
        }
    }
}
