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
use std::time::Instant;

use redis::aio::ConnectionManager;
use routers_network::Entry;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::event::VehicleId;
use crate::materializer::sink::{Applied, SegmentState, Sink, StoredLayer, merge, target_segment};
use crate::metrics::Metrics;
use crate::partition::{fnv1a, mix};
use crate::protocol::ids::{Revision, SegmentId};
use crate::protocol::output::is_final;
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
    /// A selected segment read did not return one layer/tombstone pair per timestamp.
    #[error("invalid materialized segment field reply")]
    MalformedSegmentReply,
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
/// metadata hash; both carry the same `{vehicle}` hash tag. `ARGV[1..=2]` are
/// the expected and next vehicle versions, `ARGV[3]` the mutation count,
/// `ARGV[4]` the segment's `max_ts` metadata field and `ARGV[5]` the newest
/// layer or tombstone timestamp this plan sets (`''` for none). Every following
/// four-tuple is target (`L`ayer or `M`etadata), operation (`S`et or `D`elete),
/// field, value.
///
/// `max_ts` bounds every layer and tombstone field in the segment, letting a
/// writer prove an unseen timestamp is absent without reading it. It is raised
/// when present and initialised only for a segment hash that does not exist
/// yet, so a segment populated without it is never mistaken for empty.
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

local written = ARGV[5]
if written ~= '' and not tonumber(written) then
  return redis.error_reply('invalid materializer field bound')
end

local arg = 6
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

local fresh = redis.call('EXISTS', KEYS[1]) == 0

local mutation_arg = 6
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

if written ~= '' then
  local bound = redis.call('HGET', KEYS[2], ARGV[4])
  if bound then
    if tonumber(written) > tonumber(bound) then
      redis.call('HSET', KEYS[2], ARGV[4], written)
    end
  elseif fresh then
    redis.call('HSET', KEYS[2], ARGV[4], written)
  end
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

/// The metadata field bounding every layer and tombstone timestamp in a segment.
fn max_ts_field(segment: SegmentId) -> String {
    format!("max_ts:{}", segment.0)
}

/// A fleet of independent Valkey primaries backing the materialised view,
/// addressed by vehicle. Cloning shares reconnecting connections.
#[derive(Clone)]
pub struct ValkeySink {
    conns: Vec<ConnectionManager>,
    placement: Placement,
    apply_script: Arc<redis::Script>,
    metrics: Option<Metrics>,
    cache: Option<Arc<ViewCache>>,
    /// Snapshot reads issued, so tests can prove the cache avoided them.
    #[cfg(test)]
    reads: Arc<core::sync::atomic::AtomicUsize>,
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
            let conn =
                ConnectionManager::new(redis::Client::open(endpoint.url.connection_url())?).await?;
            conns.push(conn);
        }
        Ok(Self {
            conns,
            placement: Placement::new(endpoints),
            apply_script: Arc::new(redis::Script::new(APPLY_LUA)),
            metrics: None,
            cache: None,
            #[cfg(test)]
            reads: Arc::default(),
        })
    }

    /// Plan each vehicle's next apply from this process's own last write,
    /// retaining up to `entries` vehicles. An apply it fully covers costs one
    /// round trip (the compare-and-set) instead of a read and a write; any gap
    /// or conflicting writer falls back to reading. Pair it with vehicle-stable
    /// consumer ownership, or most lookups miss.
    #[must_use]
    pub fn with_view_cache(mut self, entries: usize) -> Self {
        self.cache = (entries > 0).then(|| Arc::new(ViewCache::new(entries)));
        self
    }

    fn remember(&self, vehicle: VehicleId, view: Option<CachedView>) {
        if let (Some(cache), Some(view)) = (&self.cache, view) {
            cache.put(vehicle, view);
        }
    }

    /// Attach bounded-rate stage timing to this sink.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// The connection for a vehicle, so all its keys land on one primary.
    fn connection(&self, vehicle: VehicleId) -> ConnectionManager {
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
    /// The newest layer or tombstone timestamp this plan sets, if any.
    written_through: Option<i64>,
}

/// What a snapshot proves about a segment's layer and tombstone fields beyond
/// the timestamps it explicitly read.
///
/// [`APPLY_LUA`] maintains a per-segment `max_ts` metadata field: every layer
/// or tombstone field it writes is at or below it. The field is initialised
/// only when the script sees the segment hash does not yet exist, so a segment
/// written before the field existed stays [`Unknown`](Self::Unknown) and is
/// never assumed empty.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FieldBound {
    /// The segment has no layer or tombstone fields at all.
    Empty,
    /// No layer or tombstone field above this timestamp exists.
    Through(i64),
    /// Nothing is known beyond the timestamps explicitly read.
    Unknown,
}

impl FieldBound {
    /// Whether this bound proves no field exists at `timestamp`.
    fn excludes(self, timestamp: i64) -> bool {
        match self {
            Self::Empty => true,
            Self::Through(max) => timestamp > max,
            Self::Unknown => false,
        }
    }

    /// The bound after [`APPLY_LUA`] writes fields up to `written`.
    fn extend(self, written: Option<i64>) -> Self {
        match (self, written) {
            (bound, None) | (bound @ Self::Unknown, _) => bound,
            (Self::Empty, Some(max)) => Self::Through(max),
            (Self::Through(current), Some(max)) => Self::Through(current.max(max)),
        }
    }
}

/// The vehicle-wide and segment-scoped metadata one apply decides with.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SnapshotMeta {
    version: u64,
    had_current: bool,
    current_segment: Option<SegmentId>,
    current_revision: Option<Revision>,
    finalized_through: Option<i64>,
    last_revision: Option<Revision>,
    bound: FieldBound,
}

/// Everything one compare-and-set is planned from: metadata plus the layer and
/// tombstone revisions at the output's non-final timestamps. Layer bodies are
/// never needed: [`merge`] decides on revisions and new bodies come from the output.
struct Snapshot {
    meta: SnapshotMeta,
    layers: BTreeMap<i64, Revision>,
    retractions: BTreeMap<i64, Revision>,
}

/// The exact layer and tombstone revisions at one timestamp, as of a version.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct KnownField {
    layer: Option<Revision>,
    tombstone: Option<Revision>,
}

/// What this process last saw of one vehicle's served view, at `meta.version`.
///
/// A materializer is normally the only writer for its vehicles, so its own
/// last write is still current and the next output can be planned without a
/// read. The compare-and-set still names `meta.version`, so if another writer
/// intervened the write is refused and the apply falls back to a fresh read.
#[derive(Clone, Debug)]
struct CachedView {
    segment: SegmentId,
    meta: SnapshotMeta,
    /// Non-final timestamps whose fields are known exactly.
    known: BTreeMap<i64, KnownField>,
}

impl CachedView {
    /// A snapshot for `segment`'s `timestamps`, or `None` when some timestamp
    /// is neither final, known, nor excluded by the segment's field bound.
    fn snapshot(&self, segment: SegmentId, timestamps: &[i64]) -> Option<Snapshot> {
        if segment != self.segment {
            return None;
        }
        let mut layers = BTreeMap::new();
        let mut retractions = BTreeMap::new();
        for &timestamp in timestamps {
            if is_final(timestamp, self.meta.finalized_through) {
                // `merge` never reads or writes a final timestamp.
                continue;
            }
            match self.known.get(&timestamp) {
                Some(field) => {
                    if let Some(revision) = field.layer {
                        layers.insert(timestamp, revision);
                    }
                    if let Some(revision) = field.tombstone {
                        retractions.insert(timestamp, revision);
                    }
                }
                None if self.meta.bound.excludes(timestamp) => {}
                None => return None,
            }
        }
        Some(Snapshot {
            meta: self.meta,
            layers,
            retractions,
        })
    }
}

/// A bounded, least-recently-used map of [`CachedView`]s.
struct ViewCache {
    views: scc::HashCache<VehicleId, CachedView, rustc_hash::FxBuildHasher>,
}

impl ViewCache {
    fn new(capacity: usize) -> Self {
        Self {
            views: scc::HashCache::with_capacity_and_hasher(
                0,
                capacity.max(1),
                rustc_hash::FxBuildHasher,
            ),
        }
    }

    fn take(&self, vehicle: VehicleId) -> Option<CachedView> {
        self.views.remove(&vehicle).map(|(_, view)| view)
    }

    fn put(&self, vehicle: VehicleId, view: CachedView) {
        match self.views.entry(vehicle) {
            scc::hash_cache::Entry::Occupied(mut occupied) => {
                occupied.put(view);
            }
            scc::hash_cache::Entry::Vacant(vacant) => {
                let _ = vacant.put_entry(view);
            }
        }
    }
}

/// Metadata fields needed to merge one output and construct its CAS write.
fn snapshot_fields(segment: SegmentId) -> [String; 6] {
    [
        STATE_VERSION_FIELD.into(),
        "current_segment".into(),
        "current_revision".into(),
        finalized_field(segment),
        last_revision_field(segment),
        max_ts_field(segment),
    ]
}

/// The only timestamps whose layer or tombstone can affect this output.
fn affected_timestamps<E: Entry>(kind: &OutputKind<E>) -> Vec<i64> {
    let mut timestamps = match kind {
        OutputKind::Matched { diff, .. } => {
            diff.layers.iter().map(|layer| layer.timestamp).collect()
        }
        OutputKind::Retraction { timestamps } => timestamps.clone(),
        OutputKind::Terminal { .. } | OutputKind::Reset { .. } => Vec::new(),
    };
    timestamps.sort_unstable();
    timestamps.dedup();
    timestamps
}

impl WritePlan {
    /// Record that this plan sets a field at `timestamp`.
    fn note_written(&mut self, timestamp: i64) {
        self.written_through = Some(
            self.written_through
                .map_or(timestamp, |max| max.max(timestamp)),
        );
    }

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

/// Decode the metadata half of a read snapshot.
fn snapshot_meta(
    meta: &std::collections::HashMap<String, String>,
    segment: SegmentId,
    segment_exists: bool,
) -> Result<SnapshotMeta, ValkeyError> {
    let bound = match meta
        .get(&max_ts_field(segment))
        .and_then(|value| value.parse::<i64>().ok())
    {
        Some(max) => FieldBound::Through(max),
        None if !segment_exists => FieldBound::Empty,
        None => FieldBound::Unknown,
    };
    Ok(SnapshotMeta {
        version: state_version(meta)?,
        had_current: meta.contains_key("current_segment"),
        current_segment: meta
            .get("current_segment")
            .and_then(|value| value.parse::<u64>().ok())
            .map(SegmentId),
        current_revision: meta
            .get("current_revision")
            .and_then(|value| value.parse::<u64>().ok())
            .map(Revision),
        finalized_through: meta
            .get(&finalized_field(segment))
            .and_then(|value| value.parse::<i64>().ok()),
        last_revision: meta
            .get(&last_revision_field(segment))
            .and_then(|value| value.parse::<u64>().ok())
            .map(Revision),
        bound,
    })
}

/// Atomically install `plan` only when `expected_version` still names the
/// snapshot it was derived from. Returns `false` on a benign CAS conflict.
#[allow(clippy::too_many_arguments)]
async fn apply_plan(
    script: &redis::Script,
    conn: &mut ConnectionManager,
    layer_key: &str,
    metadata_key: &str,
    bound_field: &str,
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
        .arg(plan.len())
        .arg(bound_field)
        .arg(
            plan.written_through
                .map(|max| max.to_string())
                .unwrap_or_default(),
        );

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

/// Layer and tombstone revisions by timestamp.
type Revisions = (BTreeMap<i64, Revision>, BTreeMap<i64, Revision>);

/// A read's metadata values, segment existence, and selected segment fields.
type SnapshotReply = (Vec<Option<String>>, i64, Vec<Option<Vec<u8>>>);

/// Decode the selected layer/tombstone pairs from a segment HASH, keeping only
/// revisions. A stored layer serialises its revision first, so the layer body
/// (including its road geometry) is never decoded.
fn decode_revisions(
    timestamps: &[i64],
    raw: Vec<Option<Vec<u8>>>,
) -> Result<Revisions, ValkeyError> {
    if raw.len() != timestamps.len().saturating_mul(2) {
        return Err(ValkeyError::MalformedSegmentReply);
    }
    let mut layers = BTreeMap::new();
    let mut retractions = BTreeMap::new();
    let (pairs, _) = raw.as_chunks::<2>();
    for (timestamp, pair) in timestamps.iter().copied().zip(pairs) {
        if let Some(bytes) = &pair[0] {
            let (revision, _) = postcard::take_from_bytes::<Revision>(bytes)?;
            layers.insert(timestamp, revision);
        }
        if let Some(bytes) = &pair[1] {
            retractions.insert(timestamp, postcard::from_bytes(bytes)?);
        }
    }
    Ok((layers, retractions))
}

/// Load the versioned metadata, whether the segment hash exists, and selected
/// segment fields in one pipelined round trip. Redis executes them in order; if
/// another writer advances the version after the first read, the later CAS
/// rejects this snapshot.
async fn load_snapshot(
    conn: &mut ConnectionManager,
    vehicle: VehicleId,
    segment: SegmentId,
    timestamps: &[i64],
) -> Result<Snapshot, ValkeyError> {
    let fields = snapshot_fields(segment);
    let mut pipeline = redis::pipe();
    pipeline
        // This must remain first: the CAS version makes a later mixed
        // snapshot retry instead of overwriting concurrent state.
        .cmd("HMGET")
        .arg(meta_key(vehicle))
        .arg(&fields)
        .cmd("EXISTS")
        .arg(segment_key(vehicle, segment));
    let (values, exists, raw): SnapshotReply = if timestamps.is_empty() {
        let (values, exists): (Vec<Option<String>>, i64) = pipeline.query_async(conn).await?;
        (values, exists, Vec::new())
    } else {
        pipeline.cmd("HMGET").arg(segment_key(vehicle, segment));
        for timestamp in timestamps {
            pipeline
                .arg(timestamp.to_string())
                .arg(format!("retracted:{timestamp}"));
        }
        pipeline.query_async(conn).await?
    };
    let meta: std::collections::HashMap<String, String> = fields
        .into_iter()
        .zip(values)
        .filter_map(|(field, value)| value.map(|value| (field, value)))
        .collect();
    let meta = snapshot_meta(&meta, segment, exists > 0)?;
    let (layers, retractions) = decode_revisions(timestamps, raw)?;
    Ok(Snapshot {
        meta,
        layers,
        retractions,
    })
}

/// The write set for folding `output` into `snapshot`, and the view it leaves.
struct Planned {
    applied: Applied,
    plan: WritePlan,
    /// The metadata as of `snapshot.meta.version + 1` if `plan` is installed.
    after: SnapshotMeta,
    /// The exact post-apply fields at the output's non-final timestamps.
    fields: BTreeMap<i64, KnownField>,
}

fn plan_apply<E: Entry>(
    snapshot: Snapshot,
    output: &CommittedOutput<E>,
    segment: SegmentId,
    timestamps: &[i64],
) -> Result<Planned, ValkeyError> {
    let Snapshot {
        meta,
        layers,
        retractions,
    } = snapshot;
    let mut state = SegmentState::<E, Revision>::from_parts(
        layers,
        meta.finalized_through,
        meta.last_revision,
        retractions,
    );
    let before_layers = state.layers.clone();
    let before_retractions = state.retractions.clone();

    let applied = merge(&mut state, output);
    let mut plan = WritePlan::default();

    // New layer bodies come straight from the output: `merge` only installs
    // layers the output carried.
    let bodies: BTreeMap<i64, &crate::event::MatchedLayer<E>> = match &output.kind {
        OutputKind::Matched { diff, .. } => diff
            .layers
            .iter()
            .map(|layer| (layer.timestamp, layer))
            .collect(),
        _ => BTreeMap::new(),
    };
    for (timestamp, revision) in &state.layers {
        if before_layers.get(timestamp) != Some(revision) {
            let layer = bodies
                .get(timestamp)
                .expect("merge installs only layers the output carries");
            plan.layers.push(HashMutation::Set {
                field: timestamp.to_string(),
                value: postcard::to_allocvec(&StoredLayer {
                    revision: *revision,
                    layer: (*layer).clone(),
                })?,
            });
            plan.note_written(*timestamp);
        }
    }
    for timestamp in before_layers.keys() {
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
            plan.note_written(*timestamp);
        }
    }
    for timestamp in before_retractions.keys() {
        if !state.retractions.contains_key(timestamp) {
            plan.layers.push(HashMutation::Delete {
                field: format!("retracted:{timestamp}"),
            });
        }
    }
    if state.finalized_through != meta.finalized_through
        && let Some(through) = state.finalized_through
    {
        plan.metadata.push(HashMutation::Set {
            field: finalized_field(segment),
            value: through.to_string().into_bytes(),
        });
    }
    if state.last_revision != meta.last_revision
        && let Some(revision) = state.last_revision
    {
        plan.metadata.push(HashMutation::Set {
            field: last_revision_field(segment),
            value: revision.0.to_string().into_bytes(),
        });
    }

    // A reset moves the current segment only when it is not stale. Outputs in
    // the current segment advance its revision even when they carry no layers,
    // so a late reset cannot roll the vehicle backwards.
    let reset = matches!(output.kind, OutputKind::Reset { .. });
    let update_current = !meta.had_current
        || (reset
            && meta
                .current_revision
                .is_none_or(|current| output.revision.0 >= current.0))
        || (!reset
            && meta.current_segment == Some(segment)
            && meta
                .current_revision
                .is_none_or(|current| output.revision.0 > current.0));
    let mut after = meta;
    if update_current {
        plan.metadata.push(HashMutation::Set {
            field: "current_segment".into(),
            value: segment.0.to_string().into_bytes(),
        });
        plan.metadata.push(HashMutation::Set {
            field: "current_revision".into(),
            value: output.revision.0.to_string().into_bytes(),
        });
        after.had_current = true;
        after.current_segment = Some(segment);
        after.current_revision = Some(output.revision);
    }
    after.finalized_through = state.finalized_through;
    after.last_revision = state.last_revision;
    after.bound = meta.bound.extend(plan.written_through);
    if !plan.is_empty() {
        after.version = meta
            .version
            .checked_add(1)
            .ok_or(ValkeyError::VersionExhausted)?;
    }

    let fields = timestamps
        .iter()
        .copied()
        .filter(|timestamp| !is_final(*timestamp, meta.finalized_through))
        .map(|timestamp| {
            (
                timestamp,
                KnownField {
                    layer: state.layers.get(&timestamp).copied(),
                    tombstone: state.retractions.get(&timestamp).copied(),
                },
            )
        })
        .collect();
    Ok(Planned {
        applied,
        plan,
        after,
        fields,
    })
}

/// The view to cache after an apply at `planned.after`, carrying forward what
/// `base` knew when it describes the same segment at the snapshot's version.
fn next_view(
    base: Option<CachedView>,
    snapshot_version: u64,
    segment: SegmentId,
    planned: Planned,
) -> Option<CachedView> {
    // An unknown bound cannot prove any unseen timestamp absent; caching it
    // would never save a read.
    if planned.after.bound == FieldBound::Unknown {
        return None;
    }
    let mut known = base
        .filter(|view| view.segment == segment && view.meta.version == snapshot_version)
        .map(|view| view.known)
        .unwrap_or_default();
    known.extend(planned.fields);
    let finalized = planned.after.finalized_through;
    known.retain(|timestamp, _| !is_final(*timestamp, finalized));
    Some(CachedView {
        segment,
        meta: planned.after,
        known,
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
        let bound_field = max_ts_field(segment);
        let timestamps = affected_timestamps(&output.kind);
        let sampled = self.metrics.as_ref().is_some_and(Metrics::sample_probe);
        let mut cached = self.cache.as_ref().and_then(|cache| cache.take(vehicle));
        let record = |outcome: &'static str| {
            if let Some(metrics) = &self.metrics {
                metrics.view_cache_lookup(outcome);
            }
        };
        if self.cache.is_some() && cached.is_none() {
            record("miss");
        }

        for _ in 0..CAS_RETRIES {
            // Plan from this process's last write when it covers every affected
            // field; otherwise read. Either way the CAS names the version the
            // plan came from.
            let from_cache = cached
                .as_ref()
                .and_then(|view| view.snapshot(segment, &timestamps));
            let hit = from_cache.is_some();
            if cached.is_some() && !hit {
                record("uncovered");
            }
            let snapshot = match from_cache {
                Some(snapshot) => snapshot,
                None => {
                    let read_started = Instant::now();
                    #[cfg(test)]
                    self.reads
                        .fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                    let snapshot = load_snapshot(&mut conn, vehicle, segment, &timestamps).await?;
                    if sampled && let Some(metrics) = &self.metrics {
                        metrics.materializer_stage_seconds(
                            "valkey_read",
                            read_started.elapsed().as_secs_f64(),
                        );
                    }
                    snapshot
                }
            };
            let version = snapshot.meta.version;
            let plan_started = Instant::now();
            let planned = plan_apply(snapshot, output, segment, &timestamps)?;
            if sampled && let Some(metrics) = &self.metrics {
                metrics.materializer_stage_seconds("plan", plan_started.elapsed().as_secs_f64());
            }
            let applied = planned.applied;

            if planned.plan.is_empty() {
                // This is a true no-op: `merge` leaves the state and its
                // monotonic last revision unchanged only when this output is
                // already dominated. A concurrent writer can only advance
                // those facts, so it cannot turn this duplicate into a write.
                if hit {
                    record("hit");
                }
                self.remember(vehicle, next_view(cached, version, segment, planned));
                return Ok(applied);
            }

            let write_started = Instant::now();
            let installed = apply_plan(
                &self.apply_script,
                &mut conn,
                &layer_key,
                &meta_key,
                &bound_field,
                version,
                &planned.plan,
            )
            .await?;
            if sampled && let Some(metrics) = &self.metrics {
                metrics.materializer_stage_seconds(
                    if hit {
                        "valkey_write_cached"
                    } else {
                        "valkey_write"
                    },
                    write_started.elapsed().as_secs_f64(),
                );
            }
            if installed {
                if hit {
                    record("hit");
                }
                self.remember(vehicle, next_view(cached, version, segment, planned));
                return Ok(applied);
            }
            if hit {
                // Another writer advanced the vehicle: read afresh.
                record("conflict");
            }
            cached = None;
        }

        Err(ValkeyError::CasExhausted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::event::{MatchedDiff, MatchedLayer};
    use crate::materializer::memory::MemorySink;
    use crate::protocol::ids::{JobId, ObservationId};
    use crate::protocol::output::{ResetReason, TerminalReason};
    use geo::Point;
    use routers_network::mock::MockEntryId;
    use routers_network::{DirectionAwareEdgeId, Edge};

    fn test_output(
        vehicle: VehicleId,
        revision: u64,
        kind: OutputKind<MockEntryId>,
    ) -> CommittedOutput<MockEntryId> {
        CommittedOutput::new(
            JobId(u128::from(revision)),
            vehicle,
            ObservationId {
                partition: 0,
                sequence: revision,
            },
            Revision(revision),
            SegmentId(1),
            kind,
        )
    }

    fn matched_at(timestamp: i64, finalized_through: Option<i64>) -> OutputKind<MockEntryId> {
        OutputKind::Matched {
            diff: MatchedDiff {
                revision: 0,
                downgraded: false,
                layers: vec![MatchedLayer {
                    timestamp,
                    edge: Edge {
                        source: MockEntryId(1),
                        target: MockEntryId(2),
                        weight: 1,
                        id: DirectionAwareEdgeId::new(MockEntryId(3)),
                    },
                    position: Point::new(0.0, 0.0),
                    path: Vec::new(),
                }],
            },
            finalized_through,
        }
    }

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
                "max_ts:7",
            ]
        );
    }

    #[test]
    fn only_affected_timestamps_are_loaded_from_the_segment_hash() {
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

        assert!(affected_timestamps(&matched).is_empty());
        assert_eq!(affected_timestamps(&retraction), [1]);
        assert!(affected_timestamps(&terminal).is_empty());
        assert!(affected_timestamps(&reset).is_empty());
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
            .find("local mutation_arg = 6")
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

    fn cached_view(bound: FieldBound, finalized: Option<i64>) -> CachedView {
        let mut known = BTreeMap::new();
        known.insert(
            20,
            KnownField {
                layer: Some(Revision(4)),
                tombstone: None,
            },
        );
        known.insert(
            30,
            KnownField {
                layer: None,
                tombstone: Some(Revision(5)),
            },
        );
        CachedView {
            segment: SegmentId(1),
            meta: SnapshotMeta {
                version: 7,
                had_current: true,
                current_segment: Some(SegmentId(1)),
                current_revision: Some(Revision(5)),
                finalized_through: finalized,
                last_revision: Some(Revision(5)),
                bound,
            },
            known,
        }
    }

    #[test]
    fn a_field_bound_excludes_only_what_it_proves() {
        assert!(FieldBound::Empty.excludes(i64::MIN));
        assert!(FieldBound::Through(10).excludes(11));
        assert!(!FieldBound::Through(10).excludes(10));
        assert!(!FieldBound::Unknown.excludes(i64::MAX));
        assert_eq!(FieldBound::Empty.extend(Some(4)), FieldBound::Through(4));
        assert_eq!(
            FieldBound::Through(9).extend(Some(4)),
            FieldBound::Through(9)
        );
        assert_eq!(FieldBound::Through(9).extend(None), FieldBound::Through(9));
        assert_eq!(FieldBound::Unknown.extend(Some(4)), FieldBound::Unknown);
    }

    #[test]
    fn a_cached_view_covers_final_known_and_bounded_timestamps_only() {
        let view = cached_view(FieldBound::Through(30), Some(10));
        let snapshot = view
            .snapshot(SegmentId(1), &[5, 20, 30, 40])
            .expect("final, known, known, and above the bound");
        assert_eq!(snapshot.meta.version, 7);
        assert_eq!(snapshot.layers, BTreeMap::from([(20, Revision(4))]));
        assert_eq!(snapshot.retractions, BTreeMap::from([(30, Revision(5))]));

        assert!(
            view.snapshot(SegmentId(1), &[25]).is_none(),
            "unknown, below bound"
        );
        assert!(
            view.snapshot(SegmentId(2), &[40]).is_none(),
            "another segment"
        );
        let unknown = cached_view(FieldBound::Unknown, Some(10));
        assert!(unknown.snapshot(SegmentId(1), &[40]).is_none());
    }

    #[test]
    fn next_view_extends_the_same_version_and_prunes_final_fields() {
        let base = cached_view(FieldBound::Through(30), None);
        let mut after = base.meta;
        after.version = 8;
        after.finalized_through = Some(20);
        after.bound = FieldBound::Through(40);
        let planned = Planned {
            applied: Applied::Duplicate,
            plan: WritePlan::default(),
            after,
            fields: BTreeMap::from([(
                40,
                KnownField {
                    layer: Some(Revision(8)),
                    tombstone: None,
                },
            )]),
        };
        let view = next_view(Some(base.clone()), 7, SegmentId(1), planned).expect("cached");
        assert_eq!(view.meta.version, 8);
        assert_eq!(
            view.known.keys().copied().collect::<Vec<_>>(),
            [30, 40],
            "20 is now final; 30 carried forward; 40 learned"
        );

        let mut stale = base.meta;
        stale.version = 9;
        let planned = Planned {
            applied: Applied::Duplicate,
            plan: WritePlan::default(),
            after: stale,
            fields: BTreeMap::new(),
        };
        let view = next_view(Some(base), 8, SegmentId(1), planned).expect("cached");
        assert!(
            view.known.is_empty(),
            "a different version carries nothing forward"
        );
    }

    #[test]
    fn an_unknown_bound_is_never_cached() {
        let mut meta = cached_view(FieldBound::Unknown, None).meta;
        meta.version = 8;
        let planned = Planned {
            applied: Applied::Duplicate,
            plan: WritePlan::default(),
            after: meta,
            fields: BTreeMap::new(),
        };
        assert!(next_view(None, 7, SegmentId(1), planned).is_none());
    }

    #[test]
    fn revisions_decode_from_a_stored_layer_prefix() {
        let OutputKind::Matched { diff, .. } = matched_at(10, None) else {
            unreachable!()
        };
        let stored = StoredLayer {
            revision: Revision(42),
            layer: diff.layers[0].clone(),
        };
        let bytes = postcard::to_allocvec(&stored).unwrap();
        let tombstone = postcard::to_allocvec(&Revision(43)).unwrap();
        let (layers, retractions) =
            decode_revisions(&[10], vec![Some(bytes), Some(tombstone)]).unwrap();
        assert_eq!(layers, BTreeMap::from([(10, Revision(42))]));
        assert_eq!(retractions, BTreeMap::from([(10, Revision(43))]));
    }

    #[test]
    fn apply_script_initialises_the_bound_only_for_a_new_segment() {
        let fresh = APPLY_LUA
            .find("local fresh = redis.call('EXISTS', KEYS[1]) == 0")
            .expect("existence is checked");
        let first_write = APPLY_LUA
            .find("redis.call('HSET', key, field, value)")
            .expect("mutations");
        assert!(fresh < first_write, "existence is read before any write");
        assert!(APPLY_LUA.contains("elseif fresh then"));
    }

    /// A deterministic generator of one vehicle's committed-output stream: a
    /// sliding window of re-emitted layers, occasional retractions and resets,
    /// delivered mostly in order with redeliveries and swaps.
    fn scrambled_stream(
        vehicle: VehicleId,
        seed: u64,
        len: u64,
    ) -> Vec<CommittedOutput<MockEntryId>> {
        let mut state = seed | 1;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };
        let layer = |timestamp: i64| MatchedLayer {
            timestamp,
            edge: Edge {
                source: MockEntryId(1),
                target: MockEntryId(2),
                weight: 1,
                id: DirectionAwareEdgeId::new(MockEntryId(timestamp)),
            },
            position: Point::new(0.0, 0.0),
            path: vec![Point::new(1.0, 1.0)],
        };
        let mut outputs = Vec::new();
        let mut segment = SegmentId(1);
        let mut head = 0i64;
        let mut finalized: Option<i64> = None;
        for revision in 1..=len {
            let roll = next() % 20;
            let kind = if roll == 0 {
                segment = SegmentId(revision);
                head = 0;
                finalized = None;
                OutputKind::Reset {
                    reason: ResetReason::Gap,
                    new_segment: segment,
                }
            } else if roll < 3 && head > 2 {
                OutputKind::Retraction {
                    timestamps: vec![head * 10, (head - 1) * 10],
                }
            } else {
                head += 1;
                if next() % 3 == 0 && head > 4 {
                    finalized = Some((head - 4) * 10);
                }
                let from = finalized.map_or(1, |f| f / 10 + 1);
                OutputKind::Matched {
                    diff: MatchedDiff {
                        revision: 0,
                        downgraded: false,
                        layers: (from..=head).map(|t| layer(t * 10)).collect(),
                    },
                    finalized_through: finalized,
                }
            };
            outputs.push(CommittedOutput::new(
                JobId(u128::from(revision) << 32 | u128::from(vehicle.0 & 0xffff)),
                vehicle,
                ObservationId {
                    partition: 0,
                    sequence: revision,
                },
                Revision(revision),
                segment,
                kind,
            ));
        }
        let mut delivered = Vec::with_capacity(outputs.len() * 2);
        for index in 0..outputs.len() {
            delivered.push(outputs[index].clone());
            match next() % 10 {
                0 if index > 0 => {
                    delivered.push(outputs[index - 1 - (next() as usize % index.min(5))].clone())
                }
                1 if index > 0 => {
                    let last = delivered.len() - 1;
                    delivered.swap(last, last - 1);
                }
                _ => {}
            }
        }
        delivered
    }

    /// Every field of a vehicle's view, with the cache's own bound elided.
    async fn view_fields(
        conn: &mut redis::aio::MultiplexedConnection,
        vehicle: VehicleId,
    ) -> BTreeMap<String, BTreeMap<String, Vec<u8>>> {
        let keys: Vec<String> = redis::cmd("KEYS")
            .arg(format!("matched:{{{}}}:*", vehicle.0))
            .query_async(conn)
            .await
            .expect("keys");
        let mut view = BTreeMap::new();
        for key in keys {
            let mut fields: BTreeMap<String, Vec<u8>> = redis::cmd("HGETALL")
                .arg(&key)
                .query_async(conn)
                .await
                .expect("hash");
            fields.retain(|field, _| !field.starts_with("max_ts:"));
            let name = key.replacen(&vehicle.0.to_string(), "V", 1);
            view.insert(name, fields);
        }
        view
    }

    #[tokio::test]
    #[ignore = "requires ROUTERS_TEST_VALKEY_URL and a local disposable Valkey"]
    async fn cached_single_round_trip_applies_match_the_read_path_exactly() {
        let url = std::env::var("ROUTERS_TEST_VALKEY_URL").expect("Valkey URL");
        let endpoint: ValkeyEndpoint = format!("test={url}").parse().expect("endpoint");
        let plain = ValkeySink::connect(core::slice::from_ref(&endpoint))
            .await
            .expect("connect");
        let cached = ValkeySink::connect(core::slice::from_ref(&endpoint))
            .await
            .expect("connect")
            .with_view_cache(1024);
        // Two cached writers sharing vehicles force compare-and-set conflicts.
        let rival_a = ValkeySink::connect(core::slice::from_ref(&endpoint))
            .await
            .expect("connect")
            .with_view_cache(1024);
        let rival_b = ValkeySink::connect(core::slice::from_ref(&endpoint))
            .await
            .expect("connect")
            .with_view_cache(1024);
        let client = redis::Client::open(url).expect("client");
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("connection");
        // Unique vehicles, so this test shares a server with the others.
        let base = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("wall clock")
                .as_nanos()
                % (1 << 48),
        )
        .expect("fits")
            << 8;
        let mut used = Vec::new();

        for seed in 1..=24u64 {
            let stream = scrambled_stream(VehicleId(1), seed, 120);
            let (a, b, c) = (
                VehicleId(base + seed * 4),
                VehicleId(base + seed * 4 + 1),
                VehicleId(base + seed * 4 + 2),
            );
            used.extend([a, b, c]);
            let reference = MemorySink::<MockEntryId>::new();
            for (index, output) in stream.iter().enumerate() {
                let as_vehicle = |vehicle: VehicleId| {
                    let mut copy = output.clone();
                    copy.vehicle_id = vehicle;
                    copy
                };
                let expected = plain.apply(&as_vehicle(a)).await.expect("read path");
                let got = cached.apply(&as_vehicle(b)).await.expect("cached path");
                assert_eq!(got, expected, "seed {seed} output {index}: same decision");
                // A rival planning a no-op from a stale view may label it
                // `Duplicate` where a fresh read would count kept-final layers;
                // neither writes, so only the resulting view is compared.
                let rival = if index % 3 == 0 { &rival_a } else { &rival_b };
                rival.apply(&as_vehicle(c)).await.expect("rival path");
                reference.apply(&as_vehicle(a)).await.expect("reference");
            }
            let read = view_fields(&mut conn, a).await;
            assert_eq!(
                read,
                view_fields(&mut conn, b).await,
                "seed {seed}: cached view"
            );
            assert_eq!(
                read,
                view_fields(&mut conn, c).await,
                "seed {seed}: rival views"
            );

            // The served layers agree with the in-memory reference view.
            let view = reference.snapshot(a).expect("reference view");
            for (segment, state) in &view.segments {
                let key = format!("matched:{{V}}:{}", segment.0);
                let stored: BTreeMap<i64, Revision> = read
                    .get(&key)
                    .into_iter()
                    .flatten()
                    .filter(|(field, _)| !field.starts_with("retracted:"))
                    .map(|(field, bytes)| {
                        let (revision, _) = postcard::take_from_bytes::<Revision>(bytes).unwrap();
                        (field.parse().unwrap(), revision)
                    })
                    .collect();
                let expected: BTreeMap<i64, Revision> = state
                    .layers
                    .iter()
                    .map(|(timestamp, layer)| (*timestamp, layer.revision))
                    .collect();
                assert_eq!(stored, expected, "seed {seed} segment {}", segment.0);
            }
        }
        // Most applies skip the read.
        let fresh = ValkeySink::connect(core::slice::from_ref(&endpoint))
            .await
            .expect("connect")
            .with_view_cache(1024);
        let vehicle = VehicleId(base + 3);
        used.push(vehicle);
        let stream = scrambled_stream(vehicle, 99, 400);
        for output in &stream {
            fresh.apply(output).await.expect("apply");
        }
        let reads = fresh.reads.load(core::sync::atomic::Ordering::Relaxed);
        // This stream resets every ~20 outputs and redelivers ~10%; each of
        // those needs a read, everything else must not.
        assert!(
            reads * 5 < stream.len(),
            "{reads} reads for {} applies",
            stream.len()
        );

        for vehicle in used {
            let keys: Vec<String> = redis::cmd("KEYS")
                .arg(format!("matched:{{{}}}:*", vehicle.0))
                .query_async(&mut conn)
                .await
                .expect("keys");
            if !keys.is_empty() {
                let _: i64 = redis::cmd("DEL")
                    .arg(keys)
                    .query_async(&mut conn)
                    .await
                    .expect("cleanup");
            }
        }
    }

    #[tokio::test]
    #[ignore = "requires ROUTERS_TEST_VALKEY_URL and a local disposable Valkey"]
    async fn sparse_reads_preserve_retraction_replay_and_finality() {
        let url = std::env::var("ROUTERS_TEST_VALKEY_URL").expect("Valkey URL");
        let endpoint: ValkeyEndpoint = format!("test={url}").parse().expect("endpoint");
        let sink = ValkeySink::connect(&[endpoint]).await.expect("connect");
        let vehicle = VehicleId(
            u64::try_from(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("wall clock")
                    .as_nanos(),
            )
            .expect("test timestamp fits"),
        );

        for (revision, kind) in [
            (1, matched_at(10, None)),
            (2, matched_at(20, None)),
            (
                3,
                OutputKind::Retraction {
                    timestamps: vec![10],
                },
            ),
            (2, matched_at(10, None)),
            (4, matched_at(10, Some(10))),
            (
                5,
                OutputKind::Retraction {
                    timestamps: vec![10],
                },
            ),
        ] {
            sink.apply(&test_output(vehicle, revision, kind))
                .await
                .expect("apply");
        }

        let client = redis::Client::open(url).expect("client");
        let mut conn = client
            .get_multiplexed_async_connection()
            .await
            .expect("connection");
        let key = segment_key(vehicle, SegmentId(1));
        let fields: std::collections::HashMap<String, Vec<u8>> = redis::cmd("HGETALL")
            .arg(&key)
            .query_async(&mut conn)
            .await
            .expect("segment hash");
        assert!(fields.contains_key("10"));
        assert!(fields.contains_key("20"));
        assert!(!fields.contains_key("retracted:10"));
        let _: i64 = redis::cmd("DEL")
            .arg(key)
            .arg(meta_key(vehicle))
            .query_async(&mut conn)
            .await
            .expect("cleanup test vehicle");
    }

    #[tokio::test]
    #[ignore = "requires ROUTERS_TEST_VALKEY_URL and a local disposable Valkey"]
    async fn reconnects_after_server_closes_the_connection() {
        let url = std::env::var("ROUTERS_TEST_VALKEY_URL").expect("Valkey URL");
        let endpoint: ValkeyEndpoint = format!("test={url}").parse().expect("endpoint");
        let sink = ValkeySink::connect(&[endpoint]).await.expect("connect");
        let mut conn = sink.connection(VehicleId(1));
        let client_id: i64 = redis::cmd("CLIENT")
            .arg("ID")
            .query_async(&mut conn)
            .await
            .expect("client ID");

        let client = redis::Client::open(url).expect("client");
        let mut admin = client
            .get_multiplexed_async_connection()
            .await
            .expect("admin connection");
        let killed: i64 = redis::cmd("CLIENT")
            .arg("KILL")
            .arg("ID")
            .arg(client_id)
            .query_async(&mut admin)
            .await
            .expect("kill test connection");
        assert_eq!(killed, 1);

        for _ in 0..20 {
            let reply: redis::RedisResult<String> = redis::cmd("PING").query_async(&mut conn).await;
            if reply.as_deref() == Ok("PONG") {
                return;
            }
            tokio::time::sleep(core::time::Duration::from_millis(100)).await;
        }
        panic!("Valkey connection did not recover after the server closed it");
    }
}
