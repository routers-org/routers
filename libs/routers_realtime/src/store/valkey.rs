//! Valkey implementation of [`CheckpointStore`], backed by a fleet of
//! independent primaries.
//!
//! A vehicle's keys carry a `{vehicle:<id>}` hash tag and are placed by
//! rendezvous hashing, so all reach one primary and a resize remaps only ~`1/N`.
//! Per-vehicle atomicity comes from Lua scripts over that key group. The
//! partition index is a recovery hint that
//! [`list_prepared`](CheckpointStore::list_prepared) repairs.

use alloc::sync::Arc;
use core::ops::{Deref, DerefMut};
use core::str::FromStr;
use core::time::Duration;
use std::collections::HashMap;
use std::sync::RwLock;
use std::time::Instant;

use futures::future::try_join_all;
use redis::aio::ConnectionManager;
use thiserror::Error;
use tokio::sync::{Mutex, mpsc};
use tracing::warn;

use crate::event::VehicleId;
use crate::metrics::Metrics;
use crate::partition::{fnv1a, mix};
use crate::protocol::ids::{ObservationId, OutputId, Revision, SegmentId, token_safe};
use crate::secret::SecretUrl;
use crate::store::checkpoint::{
    CheckpointStore, CommitPhase, DirectOutcome, PartitionFrontier, PrepareOutcome, PreparedCommit,
    StoredCheckpoint, StoredCheckpointState,
};

/// Raw values returned by the checkpoint, revision-sentinel, and prepared-record pipeline.
type LoadReply = (
    HashMap<String, Vec<u8>>,
    Option<u64>,
    HashMap<String, Vec<u8>>,
);

/// The default idle lifetime of a committed checkpoint. Prepared records never
/// expire.
pub const DEFAULT_CHECKPOINT_TTL: Duration = Duration::from_secs(600);

/// The default per-primary connection timeout.
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

const INDEX_CLEANUP_CAPACITY: usize = 4_096;
const INDEX_CLEANUP_BATCH: usize = 256;

/// A failure a [`ValkeyCheckpointStore`] operation can report.
#[derive(Debug, Error)]
pub enum ValkeyError {
    /// The backing Valkey command failed (connection, timeout, or server-side).
    #[error("valkey: {0}")]
    Redis(#[from] redis::RedisError),
    /// [`ValkeyCheckpointStore::connect`] was handed an empty endpoint list.
    #[error("no valkey endpoints were supplied")]
    NoEndpoints,
    /// Two connection entries used the same stable node id.
    #[error("duplicate valkey node id {0:?}")]
    DuplicateNodeId(String),
    /// A promote or publish mark named an output that disagreed with the staged
    /// prepared record.
    #[error("prepared output mismatch: staged {staged}, got {got}")]
    OutputMismatch { staged: OutputId, got: OutputId },
    /// Promotion was attempted before the output was durably published.
    #[error("prepared output {output} has not been published")]
    NotPublished { output: OutputId },
    /// A stored hash held a field that was missing or could not be decoded back
    /// into its typed form.
    #[error("malformed stored record: {0}")]
    Malformed(String),
    /// A Lua script returned a status token this code does not recognise.
    #[error("unexpected script reply: {0:?}")]
    UnexpectedReply(Vec<String>),
}

/// A stable rendezvous member id, independent of connection credentials and
/// network addresses. It uses the same conservative token grammar as subjects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ValkeyNodeId(String);

impl ValkeyNodeId {
    /// Validate and wrap a non-empty `[A-Za-z0-9_-]+` node id.
    pub fn new(value: &str) -> Result<Self, ValkeyEndpointError> {
        if value.is_empty() {
            return Err(ValkeyEndpointError::EmptyId);
        }
        if !token_safe(value) {
            return Err(ValkeyEndpointError::UnsafeId(value.to_owned()));
        }
        Ok(Self(value.to_owned()))
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

/// One Valkey primary: a stable placement identity and its secret connection URL.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValkeyEndpoint {
    /// Stable identity retained across credential, DNS, and port changes.
    pub id: ValkeyNodeId,
    /// Secret-bearing URL used only to establish the connection.
    pub url: SecretUrl,
}

/// Why an `id=url` Valkey endpoint specification was rejected.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ValkeyEndpointError {
    /// The specification did not contain the required `=` separator.
    #[error("valkey endpoint must be ID=URL")]
    MissingSeparator,
    /// The stable node id was empty.
    #[error("valkey node id is empty")]
    EmptyId,
    /// The stable node id was not a conservative token.
    #[error("valkey node id must match [A-Za-z0-9_-]+: {0:?}")]
    UnsafeId(String),
    /// The connection URL was invalid.
    #[error("invalid valkey URL: {0}")]
    Url(#[from] url::ParseError),
}

impl FromStr for ValkeyEndpoint {
    type Err = ValkeyEndpointError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (id, url) = value
            .split_once('=')
            .ok_or(ValkeyEndpointError::MissingSeparator)?;
        Ok(Self {
            id: ValkeyNodeId::new(id)?,
            url: url.parse()?,
        })
    }
}

/// Which primary owns a key. Held apart from the connections so the mapping is
/// testable without a server.
#[derive(Clone)]
struct Placement {
    /// One hash per stable node id, so URL rotation and list reordering move no
    /// vehicle.
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

    /// Rendezvous (highest-random-weight) placement; remaps ~`1/N` of vehicles
    /// on a resize where modulo would remap nearly all.
    fn index_for(&self, key: &str) -> usize {
        let hash = fnv1a(key.as_bytes());

        self.seeds
            .iter()
            .enumerate()
            // The seed breaks ties, so the winner never depends on list order.
            .max_by_key(|(_, seed)| (mix(hash ^ **seed), **seed))
            .map(|(index, _)| index)
            .expect("fleet is non-empty, checked in ValkeyCheckpointStore::connect")
    }
}

/// The rendezvous key of a vehicle: the content of its `{vehicle:<id>}` hash
/// tag, so all its keys place onto one primary.
fn vehicle_slot(vehicle: VehicleId) -> String {
    format!("vehicle:{}", vehicle.0)
}

/// The rendezvous key of a partition's index and frontier; not vehicle-tagged.
fn partition_slot(partition: u16) -> String {
    format!("partition:{partition}")
}

/// The committed-checkpoint hash for a vehicle.
fn checkpoint_key(vehicle: VehicleId) -> String {
    format!("{{vehicle:{}}}:checkpoint", vehicle.0)
}

/// The durable last-promoted revision, retained after checkpoint payload expiry.
fn committed_revision_key(vehicle: VehicleId) -> String {
    format!("{{vehicle:{}}}:committed-revision", vehicle.0)
}

/// The prepared-commit hash for a vehicle.
fn prepared_key(vehicle: VehicleId) -> String {
    format!("{{vehicle:{}}}:prepared", vehicle.0)
}

/// The set of vehicle ids with a staged prepared commit in a partition.
fn partition_index_key(partition: u16) -> String {
    format!("partition:{partition}:prepared")
}

/// The raw-journal frontier string for a partition.
fn frontier_key(partition: u16) -> String {
    format!("partition:{partition}:frontier")
}

/// The partition's ownership epoch; placed with its frontier.
fn epoch_key(partition: u16) -> String {
    format!("partition:{partition}:epoch")
}

/// The newest partition epoch that wrote a vehicle's checkpoint; kept with the
/// vehicle's key group and never expired.
fn owner_epoch_key(vehicle: VehicleId) -> String {
    format!("{{vehicle:{}}}:owner-epoch", vehicle.0)
}

/// The stored spelling of a [`CommitPhase`].
fn phase_str(phase: CommitPhase) -> &'static str {
    match phase {
        CommitPhase::Prepared => "prepared",
        CommitPhase::Published => "published",
    }
}

/// The checkpoint hash's fields in the order the store writes them. Test-only:
/// the `PROMOTE` script writes this hash in production, and this mirror proves
/// the [`stored_from_fields`] inverse round-trips.
#[cfg(test)]
fn stored_to_fields(checkpoint: &StoredCheckpoint) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("revision", checkpoint.revision.0.to_string().into_bytes()),
        ("segment", checkpoint.segment.0.to_string().into_bytes()),
        ("bytes", checkpoint.bytes.clone()),
    ]
}

/// Decode a checkpoint hash (as [`HGETALL`](https://redis.io/commands/hgetall)
/// returns it) back into a [`StoredCheckpoint`].
fn stored_from_fields(fields: &HashMap<String, Vec<u8>>) -> Result<StoredCheckpoint, ValkeyError> {
    Ok(StoredCheckpoint {
        revision: Revision(take_u64(fields, "revision")?),
        segment: SegmentId(take_u64(fields, "segment")?),
        bytes: take_bytes(fields, "bytes")?,
    })
}

/// The prepared-commit hash's fields in the order the store writes them; the
/// [`prepare`](CheckpointStore::prepare) script reuses this order for its `ARGV`.
fn prepared_to_fields(prepared: &PreparedCommit) -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("output", prepared.output.to_string().into_bytes()),
        ("subject", prepared.output_subject.clone().into_bytes()),
        ("output_bytes", prepared.output_bytes.clone()),
        ("next_checkpoint", prepared.next_checkpoint.clone()),
        (
            "next_revision",
            prepared.next_revision.0.to_string().into_bytes(),
        ),
        (
            "next_segment",
            prepared.next_segment.0.to_string().into_bytes(),
        ),
        (
            "expected_base",
            prepared
                .expected_base
                .map(|revision| revision.0.to_string())
                .unwrap_or_default()
                .into_bytes(),
        ),
        ("phase", phase_str(prepared.phase).as_bytes().to_vec()),
        (
            "raw_partition",
            prepared.raw.partition.to_string().into_bytes(),
        ),
        (
            "raw_sequence",
            prepared.raw.sequence.to_string().into_bytes(),
        ),
    ]
}

/// Decode a prepared-commit hash back into a [`PreparedCommit`]. The inverse of
/// [`prepared_to_fields`]: an empty `expected_base` field is `None`.
fn prepared_from_fields(fields: &HashMap<String, Vec<u8>>) -> Result<PreparedCommit, ValkeyError> {
    let output = OutputId::from_str(&take_str(fields, "output")?)
        .map_err(|err| ValkeyError::Malformed(format!("output: {err}")))?;

    let expected_base = {
        let raw = take_str(fields, "expected_base")?;
        if raw.is_empty() {
            None
        } else {
            Some(Revision(raw.parse().map_err(|_| {
                ValkeyError::Malformed(format!("expected_base: {raw:?}"))
            })?))
        }
    };

    let phase = match take_str(fields, "phase")?.as_str() {
        "prepared" => CommitPhase::Prepared,
        "published" => CommitPhase::Published,
        other => return Err(ValkeyError::Malformed(format!("phase: {other:?}"))),
    };

    Ok(PreparedCommit {
        output,
        output_subject: take_str(fields, "subject")?,
        output_bytes: take_bytes(fields, "output_bytes")?,
        next_checkpoint: take_bytes(fields, "next_checkpoint")?,
        next_revision: Revision(take_u64(fields, "next_revision")?),
        next_segment: SegmentId(take_u64(fields, "next_segment")?),
        expected_base,
        phase,
        raw: ObservationId {
            partition: take_u16(fields, "raw_partition")?,
            sequence: take_u64(fields, "raw_sequence")?,
        },
    })
}

/// Take a hash field as raw bytes, erroring if it is absent.
fn take_bytes(fields: &HashMap<String, Vec<u8>>, name: &str) -> Result<Vec<u8>, ValkeyError> {
    fields
        .get(name)
        .cloned()
        .ok_or_else(|| ValkeyError::Malformed(format!("missing field {name}")))
}

/// Take a hash field as UTF-8 text.
fn take_str(fields: &HashMap<String, Vec<u8>>, name: &str) -> Result<String, ValkeyError> {
    String::from_utf8(take_bytes(fields, name)?)
        .map_err(|_| ValkeyError::Malformed(format!("field {name} is not utf-8")))
}

/// Take a hash field and parse it as a `u64`.
fn take_u64(fields: &HashMap<String, Vec<u8>>, name: &str) -> Result<u64, ValkeyError> {
    take_str(fields, name)?
        .parse()
        .map_err(|_| ValkeyError::Malformed(format!("field {name} is not a u64")))
}

/// Take a hash field and parse it as a `u16`.
fn take_u16(fields: &HashMap<String, Vec<u8>>, name: &str) -> Result<u16, ValkeyError> {
    take_str(fields, name)?
        .parse()
        .map_err(|_| ValkeyError::Malformed(format!("field {name} is not a u16")))
}

/// Compare-and-stage over a vehicle's key group. `KEYS[1]` = checkpoint hash
/// (unused, but keeps the invocation's full key group explicit), `KEYS[2]` =
/// prepared hash, `KEYS[3]` = durable committed revision;
/// `ARGV[1]` = expected base (`''` = never seen),
/// `ARGV[2..=11]` = prepared fields in [`prepared_to_fields`] order.
const PREPARE_LUA: &str = r#"
local existing = redis.call('HGET', KEYS[2], 'output')
if existing then
  if existing == ARGV[2] then
    return {'already'}
  end
  return {'busy', existing}
end
local actual = redis.call('GET', KEYS[3])
if ARGV[1] == '' then
  if actual then
    return {'conflict', actual}
  end
else
  if actual ~= ARGV[1] then
    return {'conflict', actual or ''}
  end
end
redis.call('HSET', KEYS[2],
  'output', ARGV[2],
  'subject', ARGV[3],
  'output_bytes', ARGV[4],
  'next_checkpoint', ARGV[5],
  'next_revision', ARGV[6],
  'next_segment', ARGV[7],
  'expected_base', ARGV[8],
  'phase', ARGV[9],
  'raw_partition', ARGV[10],
  'raw_sequence', ARGV[11])
return {'prepared'}
"#;

/// Install the prepared record's checkpoint and drop the prepared record.
/// `KEYS[1]` = checkpoint, `KEYS[2]` = prepared, `KEYS[3]` = durable committed
/// revision; `ARGV[1]` = output being
/// promoted, `ARGV[2]` = checkpoint TTL in ms. `{'noop'}` when already gone,
/// `{'mismatch', staged}` on a different output, and `{'not-published'}` when
/// the broker-acknowledged phase transition has not happened yet.
const PROMOTE_LUA: &str = r#"
local existing = redis.call('HGET', KEYS[2], 'output')
if not existing then
  return {'noop'}
end
if existing ~= ARGV[1] then
  return {'mismatch', existing}
end
local phase = redis.call('HGET', KEYS[2], 'phase')
if phase ~= 'published' then
  return {'not-published'}
end
local rev = redis.call('HGET', KEYS[2], 'next_revision')
local seg = redis.call('HGET', KEYS[2], 'next_segment')
local bytes = redis.call('HGET', KEYS[2], 'next_checkpoint')
redis.call('HSET', KEYS[1], 'revision', rev, 'segment', seg, 'bytes', bytes)
redis.call('PEXPIRE', KEYS[1], ARGV[2])
redis.call('SET', KEYS[3], rev)
redis.call('DEL', KEYS[2])
return {'ok'}
"#;

/// Mark the prepared record [`CommitPhase::Published`]. `KEYS[1]` = checkpoint
/// (unused, keeps the call in the key group), `KEYS[2]` = prepared, `ARGV[1]` =
/// output. `{'noop'}` when already gone, `{'mismatch', staged}` on a different
/// output.
const MARK_PUBLISHED_LUA: &str = r#"
local existing = redis.call('HGET', KEYS[2], 'output')
if not existing then
  return {'noop'}
end
if existing ~= ARGV[1] then
  return {'mismatch', existing}
end
redis.call('HSET', KEYS[2], 'phase', 'published')
return {'ok'}
"#;

/// Promote immediately after the broker has acknowledged publication. A crash
/// before this script leaves the prepared bytes for idempotent republish; a
/// crash after it sees the installed checkpoint. No intermediate store state
/// is required, so the hot commit path uses one Valkey round trip instead of
/// separate mark and promote calls.
const FINISH_PUBLISHED_LUA: &str = r#"
local existing = redis.call('HGET', KEYS[2], 'output')
if not existing then
  return {'noop'}
end
if existing ~= ARGV[1] then
  return {'mismatch', existing}
end
local rev = redis.call('HGET', KEYS[2], 'next_revision')
local seg = redis.call('HGET', KEYS[2], 'next_segment')
local bytes = redis.call('HGET', KEYS[2], 'next_checkpoint')
redis.call('HSET', KEYS[1], 'revision', rev, 'segment', seg, 'bytes', bytes)
redis.call('PEXPIRE', KEYS[1], ARGV[2])
redis.call('SET', KEYS[3], rev)
redis.call('DEL', KEYS[2])
return {'ok'}
"#;

/// Compare-and-install a checkpoint whose outputs are already broker-durable,
/// with no staged record. `KEYS[1]` = checkpoint, `KEYS[2]` = durable committed
/// revision, `KEYS[3]` = prepared hash (a staged record must be re-driven
/// first); `ARGV[1]` = expected base (`''` = never seen), `ARGV[2]` = next
/// revision, `ARGV[3]` = next segment, `ARGV[4]` = checkpoint bytes, `ARGV[5]`
/// = checkpoint TTL in ms, `ARGV[6]` = the writer's partition epoch (`''` =
/// unfenced) and `KEYS[4]` = the vehicle's owner epoch. A newer recorded
/// owner refuses the write as `{'fenced', owner}`. A stored revision already
/// equal to the next one is `{'already'}`: revisions are raw sequences, so it
/// can only be this commit.
const COMMIT_DIRECT_LUA: &str = r#"
if ARGV[6] ~= '' then
  local owner = redis.call('GET', KEYS[4])
  if owner and tonumber(owner) > tonumber(ARGV[6]) then
    return {'fenced', owner}
  end
end
local pending = redis.call('HGET', KEYS[3], 'output')
if pending then
  return {'busy', pending}
end
local actual = redis.call('GET', KEYS[2])
if actual == ARGV[2] then
  return {'already'}
end
if ARGV[1] == '' then
  if actual then
    return {'conflict', actual}
  end
elseif actual ~= ARGV[1] then
  return {'conflict', actual or ''}
end
redis.call('HSET', KEYS[1], 'revision', ARGV[2], 'segment', ARGV[3], 'bytes', ARGV[4])
redis.call('PEXPIRE', KEYS[1], ARGV[5])
redis.call('SET', KEYS[2], ARGV[2])
if ARGV[6] ~= '' then
  redis.call('SET', KEYS[4], ARGV[6])
end
return {'ok'}
"#;

/// Set a partition frontier only while the writer still holds the partition's
/// current epoch. `KEYS[1]` = epoch, `KEYS[2]` = frontier; `ARGV[1]` = epoch,
/// `ARGV[2]` = frontier sequence. Returns 1 when written, 0 when fenced.
const SET_FRONTIER_FENCED_LUA: &str = r#"
local current = redis.call('GET', KEYS[1]) or '0'
if current ~= ARGV[1] then
  return 0
end
redis.call('SET', KEYS[2], ARGV[2])
return 1
"#;

/// The Lua scripts, compiled once and shared across every clone of the store.
struct Scripts {
    prepare: redis::Script,
    promote: redis::Script,
    mark_published: redis::Script,
    finish_published: redis::Script,
    commit_direct: redis::Script,
    set_frontier_fenced: redis::Script,
}

impl Scripts {
    fn new() -> Self {
        Self {
            prepare: redis::Script::new(PREPARE_LUA),
            promote: redis::Script::new(PROMOTE_LUA),
            mark_published: redis::Script::new(MARK_PUBLISHED_LUA),
            finish_published: redis::Script::new(FINISH_PUBLISHED_LUA),
            commit_direct: redis::Script::new(COMMIT_DIRECT_LUA),
            set_frontier_fenced: redis::Script::new(SET_FRONTIER_FENCED_LUA),
        }
    }
}

/// How to reach the Valkey fleet and how long a committed checkpoint lives.
#[derive(Clone, Debug)]
pub struct ValkeyConfig {
    /// The primaries, each with a stable placement id and a connection URL.
    pub endpoints: Vec<ValkeyEndpoint>,
    /// How long a committed checkpoint survives without a fresh commit, applied
    /// as a `PEXPIRE` on every promotion. Prepared records never get a TTL.
    pub checkpoint_ttl: Duration,
    /// How long to wait for each primary's connection to establish.
    pub connect_timeout: Duration,
    /// Bounds a queued command and its response on the multiplexed connection.
    pub response_timeout: Duration,
}

impl ValkeyConfig {
    /// A config for `urls` with the default checkpoint TTL and connect timeout.
    pub fn new(endpoints: Vec<ValkeyEndpoint>) -> Self {
        Self {
            endpoints,
            checkpoint_ttl: DEFAULT_CHECKPOINT_TTL,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            response_timeout: Duration::from_secs(10),
        }
    }
}

impl Default for ValkeyConfig {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// One replaceable multiplexed connection. redis-rs retries a timed-out command
/// on the same socket, which can remain wedged; replace that socket instead.
struct ConnectionSlot {
    client: redis::Client,
    connect_timeout: Duration,
    response_timeout: Duration,
    current: RwLock<(u64, ConnectionManager)>,
    refresh: Mutex<()>,
}

impl ConnectionSlot {
    async fn new(endpoint: &ValkeyEndpoint, cfg: &ValkeyConfig) -> Result<Arc<Self>, ValkeyError> {
        let client = redis::Client::open(endpoint.url.connection_url())?;
        let conn = Self::open(&client, cfg.connect_timeout, cfg.response_timeout).await?;
        Ok(Arc::new(Self {
            client,
            connect_timeout: cfg.connect_timeout,
            response_timeout: cfg.response_timeout,
            current: RwLock::new((0, conn)),
            refresh: Mutex::new(()),
        }))
    }

    async fn open(
        client: &redis::Client,
        connect_timeout: Duration,
        response_timeout: Duration,
    ) -> Result<ConnectionManager, ValkeyError> {
        let config = redis::aio::ConnectionManagerConfig::new()
            .set_connection_timeout(connect_timeout)
            .set_response_timeout(response_timeout);
        Ok(ConnectionManager::new_with_config(client.clone(), config).await?)
    }

    fn lease(self: &Arc<Self>) -> ConnectionLease {
        let current = self
            .current
            .read()
            .expect("Valkey connection lock poisoned");
        ConnectionLease {
            slot: Arc::clone(self),
            generation: current.0,
            conn: current.1.clone(),
        }
    }

    async fn refresh_if(&self, generation: u64) {
        let _guard = self.refresh.lock().await;
        if self
            .current
            .read()
            .expect("Valkey connection lock poisoned")
            .0
            != generation
        {
            return;
        }
        match Self::open(&self.client, self.connect_timeout, self.response_timeout).await {
            Ok(conn) => {
                let mut current = self
                    .current
                    .write()
                    .expect("Valkey connection lock poisoned");
                *current = (generation.wrapping_add(1), conn);
                warn!("replaced timed-out Valkey connection");
            }
            Err(error) => warn!(%error, "could not replace timed-out Valkey connection"),
        }
    }
}

struct ConnectionLease {
    slot: Arc<ConnectionSlot>,
    generation: u64,
    conn: ConnectionManager,
}

impl Deref for ConnectionLease {
    type Target = ConnectionManager;
    fn deref(&self) -> &Self::Target {
        &self.conn
    }
}

impl DerefMut for ConnectionLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.conn
    }
}

impl ConnectionLease {
    async fn resolve<T>(&self, result: Result<T, redis::RedisError>) -> Result<T, ValkeyError> {
        match result {
            Ok(value) => Ok(value),
            Err(error) => {
                if error.is_timeout() {
                    self.slot.refresh_if(self.generation).await;
                }
                Err(error.into())
            }
        }
    }
}

/// A [`CheckpointStore`] backed by a fleet of independent Valkey primaries.
/// Cloning shares replaceable connections and compiled scripts.
#[derive(Clone)]
pub struct ValkeyCheckpointStore {
    conns: Vec<Arc<ConnectionSlot>>,
    placement: Placement,
    scripts: Arc<Scripts>,
    cfg: ValkeyConfig,
    index_cleanup: mpsc::Sender<(u16, VehicleId)>,
    metrics: Option<Metrics>,
}

impl ValkeyCheckpointStore {
    /// Open a reconnecting connection to every primary in `cfg`, concurrently.
    /// Errors on an empty endpoint list or a connection that exceeds
    /// [`ValkeyConfig::connect_timeout`].
    pub async fn connect(cfg: ValkeyConfig) -> Result<Self, ValkeyError> {
        if cfg.endpoints.is_empty() {
            return Err(ValkeyError::NoEndpoints);
        }

        let mut ids = std::collections::HashSet::with_capacity(cfg.endpoints.len());
        for endpoint in &cfg.endpoints {
            if !ids.insert(endpoint.id.clone()) {
                return Err(ValkeyError::DuplicateNodeId(
                    endpoint.id.as_str().to_owned(),
                ));
            }
        }

        let placement = Placement::new(&cfg.endpoints);

        let conns = try_join_all(
            cfg.endpoints
                .iter()
                .map(|endpoint| ConnectionSlot::new(endpoint, &cfg)),
        )
        .await?;

        let (index_cleanup, cleanup_rx) = mpsc::channel(INDEX_CLEANUP_CAPACITY);
        tokio::spawn(run_index_cleanup(
            conns.clone(),
            placement.clone(),
            cleanup_rx,
        ));

        Ok(Self {
            conns,
            placement,
            scripts: Arc::new(Scripts::new()),
            cfg,
            index_cleanup,
            metrics: None,
        })
    }

    /// Sample checkpoint command round trips separately from orchestration work.
    #[must_use]
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// The connection to the primary that owns `vehicle`'s keys; a cheap clone
    /// sharing the socket.
    fn vehicle_conn(&self, vehicle: VehicleId) -> ConnectionLease {
        self.conns[self.placement.index_for(&vehicle_slot(vehicle))].lease()
    }

    /// The connection to the primary that owns `partition`'s index and frontier;
    /// may differ from a listed vehicle's primary.
    fn partition_conn(&self, partition: u16) -> ConnectionLease {
        self.conns[self.placement.index_for(&partition_slot(partition))].lease()
    }

    fn clean_index_later(&self, partition: u16, vehicle: VehicleId) {
        let _ = self.index_cleanup.try_send((partition, vehicle));
    }
}

async fn run_index_cleanup(
    conns: Vec<Arc<ConnectionSlot>>,
    placement: Placement,
    mut rx: mpsc::Receiver<(u16, VehicleId)>,
) {
    while let Some(first) = rx.recv().await {
        let mut batch = Vec::with_capacity(INDEX_CLEANUP_BATCH);
        batch.push(first);
        while batch.len() < INDEX_CLEANUP_BATCH {
            match rx.try_recv() {
                Ok(item) => batch.push(item),
                Err(_) => break,
            }
        }

        let mut by_primary = HashMap::<usize, Vec<(u16, VehicleId)>>::new();
        for (partition, vehicle) in batch {
            let primary = placement.index_for(&partition_slot(partition));
            by_primary
                .entry(primary)
                .or_default()
                .push((partition, vehicle));
        }

        for (primary, entries) in by_primary {
            let mut pipe = redis::pipe();
            for (partition, vehicle) in entries {
                pipe.cmd("SREM")
                    .arg(partition_index_key(partition))
                    .arg(vehicle.0)
                    .ignore();
            }
            let mut conn = conns[primary].lease();
            let result: Result<(), redis::RedisError> = pipe.query_async(&mut *conn).await;
            let _ = conn.resolve(result).await;
        }
    }
}

/// Read a `prepare` reply's status token into a [`PrepareOutcome`].
fn parse_prepare_reply(reply: &[String]) -> Result<PrepareOutcome, ValkeyError> {
    match reply.first().map(String::as_str) {
        Some("prepared") => Ok(PrepareOutcome::Prepared),
        Some("already") => Ok(PrepareOutcome::AlreadyPrepared),
        Some("busy") => {
            let pending = reply
                .get(1)
                .ok_or_else(|| ValkeyError::UnexpectedReply(reply.to_vec()))?;
            let pending = OutputId::from_str(pending)
                .map_err(|err| ValkeyError::Malformed(format!("busy output {pending:?}: {err}")))?;
            Ok(PrepareOutcome::Busy { pending })
        }
        Some("conflict") => {
            let actual =
                match reply.get(1) {
                    Some(raw) if !raw.is_empty() => Some(Revision(raw.parse().map_err(|_| {
                        ValkeyError::Malformed(format!("conflict revision {raw:?}"))
                    })?)),
                    _ => None,
                };
            Ok(PrepareOutcome::Conflict { actual })
        }
        _ => Err(ValkeyError::UnexpectedReply(reply.to_vec())),
    }
}

/// Read a `commit_direct` reply's status token into a [`DirectOutcome`].
fn parse_direct_reply(reply: &[String]) -> Result<DirectOutcome, ValkeyError> {
    match reply.first().map(String::as_str) {
        Some("ok") => Ok(DirectOutcome::Committed),
        Some("already") => Ok(DirectOutcome::AlreadyCommitted),
        Some("fenced") => {
            let owner = reply
                .get(1)
                .and_then(|owner| owner.parse().ok())
                .ok_or_else(|| ValkeyError::UnexpectedReply(reply.to_vec()))?;
            Ok(DirectOutcome::Fenced { owner })
        }
        Some("busy") => {
            let pending = reply
                .get(1)
                .ok_or_else(|| ValkeyError::UnexpectedReply(reply.to_vec()))?;
            let pending = OutputId::from_str(pending)
                .map_err(|err| ValkeyError::Malformed(format!("busy output {pending:?}: {err}")))?;
            Ok(DirectOutcome::Busy { pending })
        }
        Some("conflict") => {
            let actual =
                match reply.get(1) {
                    Some(raw) if !raw.is_empty() => Some(Revision(raw.parse().map_err(|_| {
                        ValkeyError::Malformed(format!("conflict revision {raw:?}"))
                    })?)),
                    _ => None,
                };
            Ok(DirectOutcome::Conflict { actual })
        }
        _ => Err(ValkeyError::UnexpectedReply(reply.to_vec())),
    }
}

/// Read a `promote`/`mark_published` reply: success on `ok`/`noop`, an
/// [`ValkeyError::OutputMismatch`] on `mismatch`.
fn parse_ok_or_mismatch(reply: &[String], got: OutputId) -> Result<(), ValkeyError> {
    match reply.first().map(String::as_str) {
        Some("ok" | "noop") => Ok(()),
        Some("mismatch") => {
            let staged = reply
                .get(1)
                .ok_or_else(|| ValkeyError::UnexpectedReply(reply.to_vec()))?;
            let staged = OutputId::from_str(staged).map_err(|err| {
                ValkeyError::Malformed(format!("staged output {staged:?}: {err}"))
            })?;
            Err(ValkeyError::OutputMismatch { staged, got })
        }
        _ => Err(ValkeyError::UnexpectedReply(reply.to_vec())),
    }
}

/// Read a promotion reply, additionally enforcing the publish-before-promote
/// state transition inside the atomic script.
fn parse_promote_reply(reply: &[String], got: OutputId) -> Result<(), ValkeyError> {
    match reply.first().map(String::as_str) {
        Some("not-published") => Err(ValkeyError::NotPublished { output: got }),
        _ => parse_ok_or_mismatch(reply, got),
    }
}

impl CheckpointStore for ValkeyCheckpointStore {
    type Error = ValkeyError;

    async fn load(
        &self,
        vehicle: VehicleId,
    ) -> Result<(StoredCheckpointState, Option<PreparedCommit>), Self::Error> {
        let mut conn = self.vehicle_conn(vehicle);

        // One hash-slot and an atomic pipeline give recovery a point-in-time
        // view of the checkpoint, revision sentinel, and prepared record.
        let mut pipe = redis::pipe();
        pipe.atomic()
            .cmd("HGETALL")
            .arg(checkpoint_key(vehicle))
            .cmd("GET")
            .arg(committed_revision_key(vehicle))
            .cmd("HGETALL")
            .arg(prepared_key(vehicle));
        let result = pipe.query_async(&mut *conn).await;
        let (checkpoint_fields, committed_revision, prepared_fields): LoadReply =
            conn.resolve(result).await?;

        let checkpoint = if checkpoint_fields.is_empty() {
            committed_revision.map_or(StoredCheckpointState::NeverSeen, |revision| {
                StoredCheckpointState::Expired {
                    revision: Revision(revision),
                }
            })
        } else {
            StoredCheckpointState::Present(stored_from_fields(&checkpoint_fields)?)
        };
        let prepared = if prepared_fields.is_empty() {
            None
        } else {
            Some(prepared_from_fields(&prepared_fields)?)
        };
        Ok((checkpoint, prepared))
    }

    async fn prepare(
        &self,
        vehicle: VehicleId,
        partition: u16,
        prepared: PreparedCommit,
    ) -> Result<PrepareOutcome, Self::Error> {
        let expected = prepared
            .expected_base
            .map(|revision| revision.0.to_string())
            .unwrap_or_default();
        let fields = prepared_to_fields(&prepared);

        let mut conn = self.vehicle_conn(vehicle);
        let mut invocation = self.scripts.prepare.prepare_invoke();
        invocation
            .key(checkpoint_key(vehicle))
            .key(prepared_key(vehicle))
            .key(committed_revision_key(vehicle))
            .arg(expected);
        for (_, value) in &fields {
            invocation.arg(value.as_slice());
        }
        let mut index_conn = self.partition_conn(partition);
        let mut index = redis::cmd("SADD");
        index.arg(partition_index_key(partition)).arg(vehicle.0);
        let sampled = self.metrics.as_ref().is_some_and(Metrics::sample_probe);
        let (reply, _): (Vec<String>, i64) = tokio::try_join!(
            async {
                let started = Instant::now();
                let result = invocation.invoke_async(&mut *conn).await;
                if sampled && let Some(metrics) = &self.metrics {
                    metrics.commit_stage_seconds(
                        "valkey_prepare_lua",
                        started.elapsed().as_secs_f64(),
                    );
                }
                conn.resolve(result).await
            },
            async {
                let started = Instant::now();
                let result = index.query_async(&mut *index_conn).await;
                if sampled && let Some(metrics) = &self.metrics {
                    metrics.commit_stage_seconds(
                        "valkey_prepare_index",
                        started.elapsed().as_secs_f64(),
                    );
                }
                index_conn.resolve(result).await
            },
        )?;
        let outcome = parse_prepare_reply(&reply)?;

        if matches!(outcome, PrepareOutcome::Conflict { .. }) {
            self.clean_index_later(partition, vehicle);
        }
        Ok(outcome)
    }

    async fn mark_published(
        &self,
        vehicle: VehicleId,
        output: OutputId,
    ) -> Result<(), Self::Error> {
        let mut conn = self.vehicle_conn(vehicle);
        let mut invocation = self.scripts.mark_published.prepare_invoke();
        invocation
            .key(checkpoint_key(vehicle))
            .key(prepared_key(vehicle))
            .arg(output.to_string());
        let result = invocation.invoke_async(&mut *conn).await;
        let reply: Vec<String> = conn.resolve(result).await?;
        parse_ok_or_mismatch(&reply, output)
    }

    async fn promote(
        &self,
        vehicle: VehicleId,
        partition: u16,
        output: OutputId,
    ) -> Result<(), Self::Error> {
        // Saturate rather than wrap the u128 millis into the script's u64 arg.
        let ttl_ms = u64::try_from(self.cfg.checkpoint_ttl.as_millis()).unwrap_or(u64::MAX);

        let mut conn = self.vehicle_conn(vehicle);
        let mut invocation = self.scripts.promote.prepare_invoke();
        invocation
            .key(checkpoint_key(vehicle))
            .key(prepared_key(vehicle))
            .key(committed_revision_key(vehicle))
            .arg(output.to_string())
            .arg(ttl_ms);
        let result = invocation.invoke_async(&mut *conn).await;
        let reply: Vec<String> = conn.resolve(result).await?;
        parse_promote_reply(&reply, output)?;

        self.clean_index_later(partition, vehicle);
        Ok(())
    }

    async fn finish_published(
        &self,
        vehicle: VehicleId,
        partition: u16,
        output: OutputId,
    ) -> Result<(), Self::Error> {
        let ttl_ms = u64::try_from(self.cfg.checkpoint_ttl.as_millis()).unwrap_or(u64::MAX);

        let mut conn = self.vehicle_conn(vehicle);
        let mut invocation = self.scripts.finish_published.prepare_invoke();
        invocation
            .key(checkpoint_key(vehicle))
            .key(prepared_key(vehicle))
            .key(committed_revision_key(vehicle))
            .arg(output.to_string())
            .arg(ttl_ms);
        let sampled = self.metrics.as_ref().is_some_and(Metrics::sample_probe);
        let started = Instant::now();
        let result = invocation.invoke_async(&mut *conn).await;
        if sampled && let Some(metrics) = &self.metrics {
            metrics.commit_stage_seconds("valkey_promote_lua", started.elapsed().as_secs_f64());
        }
        let reply: Vec<String> = conn.resolve(result).await?;
        parse_ok_or_mismatch(&reply, output)?;

        self.clean_index_later(partition, vehicle);
        Ok(())
    }

    async fn commit_direct(
        &self,
        vehicle: VehicleId,
        expected_base: Option<Revision>,
        checkpoint: StoredCheckpoint,
        epoch: Option<u64>,
    ) -> Result<DirectOutcome, Self::Error> {
        let ttl_ms = u64::try_from(self.cfg.checkpoint_ttl.as_millis()).unwrap_or(u64::MAX);
        let expected = expected_base
            .map(|revision| revision.0.to_string())
            .unwrap_or_default();

        let mut conn = self.vehicle_conn(vehicle);
        let mut invocation = self.scripts.commit_direct.prepare_invoke();
        invocation
            .key(checkpoint_key(vehicle))
            .key(committed_revision_key(vehicle))
            .key(prepared_key(vehicle))
            .key(owner_epoch_key(vehicle))
            .arg(expected)
            .arg(checkpoint.revision.0)
            .arg(checkpoint.segment.0)
            .arg(checkpoint.bytes.as_slice())
            .arg(ttl_ms)
            .arg(epoch.map(|epoch| epoch.to_string()).unwrap_or_default());
        let sampled = self.metrics.as_ref().is_some_and(Metrics::sample_probe);
        let started = Instant::now();
        let result = invocation.invoke_async(&mut *conn).await;
        if sampled && let Some(metrics) = &self.metrics {
            metrics
                .commit_stage_seconds("valkey_commit_direct_lua", started.elapsed().as_secs_f64());
        }
        let reply: Vec<String> = conn.resolve(result).await?;
        parse_direct_reply(&reply)
    }

    async fn list_prepared(
        &self,
        partition: u16,
    ) -> Result<Vec<(VehicleId, PreparedCommit)>, Self::Error> {
        let index_key = partition_index_key(partition);
        let mut index_conn = self.partition_conn(partition);
        let result = redis::cmd("SMEMBERS")
            .arg(&index_key)
            .query_async(&mut *index_conn)
            .await;
        let ids: Vec<u64> = index_conn.resolve(result).await?;

        let mut prepared = Vec::with_capacity(ids.len());
        let mut stale = Vec::new();
        for id in ids {
            let vehicle = VehicleId(id);
            let mut conn = self.vehicle_conn(vehicle);
            let result = redis::cmd("HGETALL")
                .arg(prepared_key(vehicle))
                .query_async(&mut *conn)
                .await;
            let fields: HashMap<String, Vec<u8>> = conn.resolve(result).await?;
            if fields.is_empty() {
                stale.push(vehicle);
            } else {
                prepared.push((vehicle, prepared_from_fields(&fields)?));
            }
        }

        // Best-effort repair; a failure is reconciled on the next recovery pass.
        for vehicle in stale {
            let result: Result<i64, redis::RedisError> = redis::cmd("SREM")
                .arg(&index_key)
                .arg(vehicle.0)
                .query_async(&mut *index_conn)
                .await;
            let _ = index_conn.resolve(result).await;
        }

        // Deterministic order, independent of set iteration.
        prepared.sort_by_key(|(vehicle, _)| vehicle.0);
        Ok(prepared)
    }

    async fn acquire_partition(&self, partition: u16) -> Result<u64, Self::Error> {
        let mut conn = self.partition_conn(partition);
        let result = redis::cmd("INCR")
            .arg(epoch_key(partition))
            .query_async(&mut *conn)
            .await;
        conn.resolve(result).await
    }

    async fn set_frontier_fenced(
        &self,
        frontier: PartitionFrontier,
        epoch: u64,
    ) -> Result<bool, Self::Error> {
        let mut conn = self.partition_conn(frontier.partition);
        let mut invocation = self.scripts.set_frontier_fenced.prepare_invoke();
        invocation
            .key(epoch_key(frontier.partition))
            .key(frontier_key(frontier.partition))
            .arg(epoch)
            .arg(frontier.sequence);
        let result = invocation.invoke_async(&mut *conn).await;
        let written: i64 = conn.resolve(result).await?;
        Ok(written == 1)
    }

    async fn frontier(&self, partition: u16) -> Result<Option<PartitionFrontier>, Self::Error> {
        let mut conn = self.partition_conn(partition);
        let result = redis::cmd("GET")
            .arg(frontier_key(partition))
            .query_async(&mut *conn)
            .await;
        let sequence: Option<u64> = conn.resolve(result).await?;
        Ok(sequence.map(|sequence| PartitionFrontier {
            partition,
            sequence,
        }))
    }

    async fn set_frontier(&self, frontier: PartitionFrontier) -> Result<(), Self::Error> {
        let mut conn = self.partition_conn(frontier.partition);
        let result = redis::cmd("SET")
            .arg(frontier_key(frontier.partition))
            .arg(frontier.sequence)
            .query_async::<()>(&mut *conn)
            .await;
        conn.resolve(result).await?;
        Ok(())
    }

    async fn expire_idle(&self, vehicle: VehicleId) -> Result<(), Self::Error> {
        // Never touch a prepared record: an in-flight commit must survive.
        let mut conn = self.vehicle_conn(vehicle);
        let result = redis::cmd("DEL")
            .arg(checkpoint_key(vehicle))
            .query_async::<i64>(&mut *conn)
            .await;
        conn.resolve(result).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fleet(n: usize) -> Vec<ValkeyEndpoint> {
        (0..n)
            .map(|i| {
                format!("node-{i:03}=redis://valkey-{i:03}:6379")
                    .parse()
                    .unwrap()
            })
            .collect()
    }

    fn slots(n: usize) -> Vec<String> {
        (0..n).map(|i| format!("vehicle:{i}")).collect()
    }

    // --- key derivation -----------------------------------------------------

    #[test]
    fn keys_are_hash_tagged_per_vehicle() {
        let vehicle = VehicleId(42);
        assert_eq!(checkpoint_key(vehicle), "{vehicle:42}:checkpoint");
        assert_eq!(
            committed_revision_key(vehicle),
            "{vehicle:42}:committed-revision"
        );
        assert_eq!(prepared_key(vehicle), "{vehicle:42}:prepared");
        assert_eq!(vehicle_slot(vehicle), "vehicle:42");
    }

    #[test]
    fn partition_keys_are_not_vehicle_tagged() {
        assert_eq!(partition_index_key(7), "partition:7:prepared");
        assert_eq!(frontier_key(7), "partition:7:frontier");
        assert_eq!(partition_slot(7), "partition:7");
    }

    // --- placement ----------------------------------------------------------

    #[test]
    fn placement_is_deterministic() {
        let urls = fleet(20);
        let (a, b) = (Placement::new(&urls), Placement::new(&urls));
        for slot in slots(1000) {
            assert_eq!(a.index_for(&slot), b.index_for(&slot));
        }
    }

    #[test]
    fn placement_ignores_url_order() {
        let urls = fleet(20);
        let mut shuffled = urls.clone();
        shuffled.reverse();
        let (direct, reversed) = (Placement::new(&urls), Placement::new(&shuffled));
        for slot in slots(1000) {
            let expected = &urls[direct.index_for(&slot)].id;
            let actual = &shuffled[reversed.index_for(&slot)].id;
            assert_eq!(expected, actual, "{slot} moved when the list was reordered");
        }
    }

    #[test]
    fn placement_survives_connection_url_rotation() {
        let before = fleet(8);
        let mut after = before.clone();
        for (index, endpoint) in after.iter_mut().enumerate() {
            endpoint.url = format!("redis://new-password@replacement-{index}:6380")
                .parse()
                .unwrap();
        }
        let (before, after) = (Placement::new(&before), Placement::new(&after));
        for slot in slots(1_000) {
            assert_eq!(before.index_for(&slot), after.index_for(&slot));
        }
    }

    #[test]
    fn endpoint_parser_requires_a_valid_stable_id() {
        assert!(
            "primary=redis://localhost:6379"
                .parse::<ValkeyEndpoint>()
                .is_ok()
        );
        assert!("redis://localhost:6379".parse::<ValkeyEndpoint>().is_err());
        assert!(
            "bad.id=redis://localhost:6379"
                .parse::<ValkeyEndpoint>()
                .is_err()
        );
    }

    #[test]
    fn growing_the_fleet_moves_about_one_nth_of_keys() {
        let before = Placement::new(&fleet(20));
        let after = Placement::new(&fleet(21));
        let sample = slots(10_000);
        let moved = sample
            .iter()
            .filter(|slot| before.index_for(slot) != after.index_for(slot))
            .count();
        let ratio = moved as f64 / sample.len() as f64;
        assert!(ratio < 0.10, "{:.1}% of keys moved", ratio * 100.0);
    }

    #[test]
    fn placement_spreads_across_the_fleet() {
        let size = 20;
        let placement = Placement::new(&fleet(size));
        let mut counts = vec![0usize; size];
        for slot in slots(20_000) {
            counts[placement.index_for(&slot)] += 1;
        }
        for (index, count) in counts.iter().enumerate() {
            assert!(
                (500..2000).contains(count),
                "primary {index} took {count} of 20000 keys"
            );
        }
    }

    #[test]
    fn a_vehicles_keys_and_index_key_place_independently() {
        let placement = Placement::new(&fleet(8));
        for id in 0..256u64 {
            let vehicle = VehicleId(id);
            let node = placement.index_for(&vehicle_slot(vehicle));
            assert!(node < 8);
        }
    }

    // --- hash-field round trips --------------------------------------------

    fn to_map(fields: Vec<(&'static str, Vec<u8>)>) -> HashMap<String, Vec<u8>> {
        fields
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect()
    }

    fn sample_prepared(expected_base: Option<Revision>, phase: CommitPhase) -> PreparedCommit {
        PreparedCommit {
            output: OutputId(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210),
            output_subject: "events.matched.v1.p.5".to_owned(),
            output_bytes: vec![0x00, 0xff, 0x10, 0x7f],
            next_checkpoint: vec![9, 8, 7, 6, 5],
            next_revision: Revision(4321),
            next_segment: SegmentId(99),
            expected_base,
            phase,
            raw: ObservationId {
                partition: 5,
                sequence: 4321,
            },
        }
    }

    #[test]
    fn stored_checkpoint_round_trips_through_fields() {
        let checkpoint = StoredCheckpoint {
            revision: Revision(1_700_000),
            segment: SegmentId(42),
            // Non-UTF-8 bytes, to prove `bytes` survives verbatim.
            bytes: vec![0x00, 0x01, 0xff, 0xfe, 0x80],
        };
        let decoded = stored_from_fields(&to_map(stored_to_fields(&checkpoint))).unwrap();
        assert_eq!(decoded, checkpoint);
    }

    #[test]
    fn prepared_commit_round_trips_with_and_without_a_base() {
        for expected_base in [None, Some(Revision(4320))] {
            for phase in [CommitPhase::Prepared, CommitPhase::Published] {
                let prepared = sample_prepared(expected_base, phase);
                let decoded = prepared_from_fields(&to_map(prepared_to_fields(&prepared))).unwrap();
                assert_eq!(decoded, prepared, "base {expected_base:?}, phase {phase:?}");
            }
        }
    }

    #[test]
    fn expected_base_none_encodes_as_empty_and_decodes_back() {
        let fields = to_map(prepared_to_fields(&sample_prepared(
            None,
            CommitPhase::Prepared,
        )));
        assert_eq!(fields.get("expected_base").unwrap(), b"");
        assert_eq!(prepared_from_fields(&fields).unwrap().expected_base, None,);
    }

    #[test]
    fn from_fields_reports_a_missing_field() {
        let mut fields = to_map(prepared_to_fields(&sample_prepared(
            None,
            CommitPhase::Prepared,
        )));
        fields.remove("next_revision");
        let err = prepared_from_fields(&fields).unwrap_err();
        assert!(matches!(err, ValkeyError::Malformed(_)), "{err:?}");
    }

    #[test]
    fn from_fields_rejects_an_unknown_phase() {
        let mut fields = to_map(prepared_to_fields(&sample_prepared(
            None,
            CommitPhase::Prepared,
        )));
        fields.insert("phase".to_owned(), b"committed".to_vec());
        let err = prepared_from_fields(&fields).unwrap_err();
        assert!(matches!(err, ValkeyError::Malformed(_)), "{err:?}");
    }

    // --- reply parsing ------------------------------------------------------

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn prepare_reply_maps_every_status() {
        assert_eq!(
            parse_prepare_reply(&strings(&["prepared"])).unwrap(),
            PrepareOutcome::Prepared
        );
        assert_eq!(
            parse_prepare_reply(&strings(&["already"])).unwrap(),
            PrepareOutcome::AlreadyPrepared
        );
        assert_eq!(
            parse_prepare_reply(&strings(&["busy", &OutputId(0xabc).to_string()])).unwrap(),
            PrepareOutcome::Busy {
                pending: OutputId(0xabc),
            }
        );
        assert_eq!(
            parse_prepare_reply(&strings(&["conflict", "77"])).unwrap(),
            PrepareOutcome::Conflict {
                actual: Some(Revision(77)),
            }
        );
        // Empty actual means no checkpoint existed.
        assert_eq!(
            parse_prepare_reply(&strings(&["conflict", ""])).unwrap(),
            PrepareOutcome::Conflict { actual: None }
        );
        assert!(matches!(
            parse_prepare_reply(&strings(&["wat"])),
            Err(ValkeyError::UnexpectedReply(_))
        ));
    }

    #[test]
    fn ok_or_mismatch_maps_every_status() {
        let got = OutputId(0x1);
        assert!(parse_ok_or_mismatch(&strings(&["ok"]), got).is_ok());
        assert!(parse_ok_or_mismatch(&strings(&["noop"]), got).is_ok());
        let staged = OutputId(0x2);
        let err =
            parse_ok_or_mismatch(&strings(&["mismatch", &staged.to_string()]), got).unwrap_err();
        assert!(matches!(
            err,
            ValkeyError::OutputMismatch { staged: s, got: g } if s == staged && g == got
        ));
        assert!(matches!(
            parse_ok_or_mismatch(&strings(&["nope"]), got),
            Err(ValkeyError::UnexpectedReply(_))
        ));
    }

    #[test]
    fn direct_reply_maps_every_status() {
        assert_eq!(
            parse_direct_reply(&strings(&["ok"])).unwrap(),
            DirectOutcome::Committed
        );
        assert_eq!(
            parse_direct_reply(&strings(&["already"])).unwrap(),
            DirectOutcome::AlreadyCommitted
        );
        assert_eq!(
            parse_direct_reply(&strings(&["conflict", "12"])).unwrap(),
            DirectOutcome::Conflict {
                actual: Some(Revision(12))
            }
        );
        assert_eq!(
            parse_direct_reply(&strings(&["conflict", ""])).unwrap(),
            DirectOutcome::Conflict { actual: None }
        );
        assert_eq!(
            parse_direct_reply(&strings(&["busy", &OutputId(0xabc).to_string()])).unwrap(),
            DirectOutcome::Busy {
                pending: OutputId(0xabc)
            }
        );
        assert_eq!(
            parse_direct_reply(&strings(&["fenced", "7"])).unwrap(),
            DirectOutcome::Fenced { owner: 7 }
        );
        assert!(parse_direct_reply(&strings(&["wat"])).is_err());
    }

    #[test]
    fn commit_direct_lua_references_its_keys_and_argv() {
        for key in 1..=4 {
            assert!(COMMIT_DIRECT_LUA.contains(&format!("KEYS[{key}]")));
        }
        for arg in 1..=6 {
            assert!(COMMIT_DIRECT_LUA.contains(&format!("ARGV[{arg}]")));
        }
        assert!(COMMIT_DIRECT_LUA.contains("PEXPIRE"));
    }

    #[test]
    fn promotion_reply_rejects_an_unpublished_record() {
        let output = OutputId(0x1);
        assert!(matches!(
            parse_promote_reply(&strings(&["not-published"]), output),
            Err(ValkeyError::NotPublished { output: got }) if got == output
        ));
        assert!(PROMOTE_LUA.contains("phase ~= 'published'"));
    }

    // --- Lua argument-layout guards ----------------------------------------

    #[test]
    fn prepare_lua_references_its_required_keys_and_argv() {
        assert!(PREPARE_LUA.contains("KEYS[2]"));
        assert!(PREPARE_LUA.contains("KEYS[3]"));
        for n in 1..=11 {
            assert!(
                PREPARE_LUA.contains(&format!("ARGV[{n}]")),
                "PREPARE is missing ARGV[{n}]"
            );
        }
        assert_eq!(
            prepared_to_fields(&sample_prepared(None, CommitPhase::Prepared)).len(),
            10
        );
        for (name, _) in prepared_to_fields(&sample_prepared(None, CommitPhase::Prepared)) {
            assert!(
                PREPARE_LUA.contains(&format!("'{name}'")),
                "PREPARE never writes field {name}"
            );
        }
    }

    #[test]
    fn promote_lua_references_its_keys_and_argv() {
        assert!(PROMOTE_LUA.contains("KEYS[1]"));
        assert!(PROMOTE_LUA.contains("KEYS[2]"));
        assert!(PROMOTE_LUA.contains("KEYS[3]"));
        assert!(PROMOTE_LUA.contains("ARGV[1]"));
        assert!(PROMOTE_LUA.contains("ARGV[2]"));
        assert!(PROMOTE_LUA.contains("PEXPIRE"));
        assert!(PROMOTE_LUA.contains("redis.call('SET', KEYS[3], rev)"));
        for field in ["next_revision", "next_segment", "next_checkpoint"] {
            assert!(PROMOTE_LUA.contains(&format!("'{field}'")));
        }
    }

    #[test]
    fn mark_published_lua_references_the_prepared_key_and_output() {
        assert!(MARK_PUBLISHED_LUA.contains("KEYS[2]"));
        assert!(MARK_PUBLISHED_LUA.contains("ARGV[1]"));
        assert!(MARK_PUBLISHED_LUA.contains("'published'"));
    }

    // --- config -------------------------------------------------------------

    #[test]
    fn config_defaults_are_ten_minutes_and_five_seconds() {
        let cfg = ValkeyConfig::new(fleet(1));
        assert_eq!(cfg.checkpoint_ttl, Duration::from_secs(600));
        assert_eq!(cfg.connect_timeout, Duration::from_secs(5));
        assert_eq!(cfg.response_timeout, Duration::from_secs(10));
    }

    #[tokio::test]
    #[ignore = "requires ROUTERS_TEST_VALKEY_URL and a local disposable Valkey"]
    async fn checkpoint_store_reconnects_after_server_closes_the_connection() {
        let url = std::env::var("ROUTERS_TEST_VALKEY_URL").expect("Valkey URL");
        let endpoint: ValkeyEndpoint = format!("test={url}").parse().expect("endpoint");
        let store = ValkeyCheckpointStore::connect(ValkeyConfig::new(vec![endpoint]))
            .await
            .expect("connect");
        let mut conn = store.partition_conn(1);
        let client_id: i64 = redis::cmd("CLIENT")
            .arg("ID")
            .query_async(&mut *conn)
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
            if store.frontier(1).await.is_ok() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("checkpoint store did not recover after the server closed its connection");
    }

    #[tokio::test]
    #[ignore = "requires ROUTERS_TEST_VALKEY_URL and a local disposable Valkey"]
    async fn commit_direct_round_trips_against_valkey() {
        let url = std::env::var("ROUTERS_TEST_VALKEY_URL").expect("Valkey URL");
        let endpoint: ValkeyEndpoint = format!("test={url}").parse().expect("endpoint");
        let store = ValkeyCheckpointStore::connect(ValkeyConfig::new(vec![endpoint]))
            .await
            .expect("connect");
        let vehicle = VehicleId(0xD1_EC7);
        let stored = |revision: u64| StoredCheckpoint {
            revision: Revision(revision),
            segment: SegmentId(3),
            bytes: vec![0x00, 0xff, revision as u8],
        };
        let mut conn = store.vehicle_conn(vehicle);
        let _: () = redis::cmd("DEL")
            .arg(checkpoint_key(vehicle))
            .arg(committed_revision_key(vehicle))
            .arg(prepared_key(vehicle))
            .query_async(&mut *conn)
            .await
            .expect("clean slate");

        assert_eq!(
            store
                .commit_direct(vehicle, None, stored(10), None)
                .await
                .unwrap(),
            DirectOutcome::Committed
        );
        assert_eq!(
            store
                .commit_direct(vehicle, None, stored(10), None)
                .await
                .unwrap(),
            DirectOutcome::AlreadyCommitted
        );
        assert_eq!(
            store
                .commit_direct(vehicle, Some(Revision(9)), stored(11), None)
                .await
                .unwrap(),
            DirectOutcome::Conflict {
                actual: Some(Revision(10))
            }
        );
        assert_eq!(
            store
                .commit_direct(vehicle, Some(Revision(10)), stored(11), None)
                .await
                .unwrap(),
            DirectOutcome::Committed
        );
        let (checkpoint, prepared) = store.load(vehicle).await.unwrap();
        assert_eq!(checkpoint, StoredCheckpointState::Present(stored(11)));
        assert!(prepared.is_none());
        let ttl: i64 = redis::cmd("PTTL")
            .arg(checkpoint_key(vehicle))
            .query_async(&mut *conn)
            .await
            .unwrap();
        assert!(ttl > 0, "the checkpoint carries its idle TTL");

        // A staged record from a staged-mode predecessor blocks direct commits.
        let staged = PreparedCommit {
            output: OutputId(0xabc),
            output_subject: "events.matched.v1.p.1".to_owned(),
            output_bytes: vec![1],
            next_checkpoint: vec![2],
            next_revision: Revision(12),
            next_segment: SegmentId(3),
            expected_base: Some(Revision(11)),
            phase: CommitPhase::Prepared,
            raw: ObservationId {
                partition: 1,
                sequence: 12,
            },
        };
        assert_eq!(
            store.prepare(vehicle, 1, staged).await.unwrap(),
            PrepareOutcome::Prepared
        );
        assert_eq!(
            store
                .commit_direct(vehicle, Some(Revision(11)), stored(13), None)
                .await
                .unwrap(),
            DirectOutcome::Busy {
                pending: OutputId(0xabc)
            }
        );
        let _: () = redis::cmd("DEL")
            .arg(checkpoint_key(vehicle))
            .arg(committed_revision_key(vehicle))
            .arg(prepared_key(vehicle))
            .arg(partition_index_key(1))
            .query_async(&mut *conn)
            .await
            .expect("cleanup");

        // Ownership: a newer partition epoch fences the older owner's writes.
        let partition = 1_000 + (vehicle.0 % 16) as u16;
        let old = store.acquire_partition(partition).await.unwrap();
        let new = store.acquire_partition(partition).await.unwrap();
        assert_eq!(new, old + 1);
        assert_eq!(
            store
                .commit_direct(vehicle, None, stored(20), Some(new))
                .await
                .unwrap(),
            DirectOutcome::Committed
        );
        assert_eq!(
            store
                .commit_direct(vehicle, Some(Revision(20)), stored(21), Some(old))
                .await
                .unwrap(),
            DirectOutcome::Fenced { owner: new }
        );
        let frontier = PartitionFrontier {
            partition,
            sequence: 99,
        };
        assert!(!store.set_frontier_fenced(frontier, old).await.unwrap());
        assert!(store.set_frontier_fenced(frontier, new).await.unwrap());
        assert_eq!(store.frontier(partition).await.unwrap(), Some(frontier));
        let _: () = redis::cmd("DEL")
            .arg(checkpoint_key(vehicle))
            .arg(committed_revision_key(vehicle))
            .arg(owner_epoch_key(vehicle))
            .arg(epoch_key(partition))
            .arg(frontier_key(partition))
            .query_async(&mut *conn)
            .await
            .expect("cleanup");
    }

    #[tokio::test]
    #[ignore = "requires ROUTERS_TEST_VALKEY_URL and a local disposable Valkey"]
    async fn checkpoint_store_replaces_timed_out_socket() {
        let url = std::env::var("ROUTERS_TEST_VALKEY_URL").expect("Valkey URL");
        let endpoint: ValkeyEndpoint = format!("test={url}").parse().expect("endpoint");
        let store = ValkeyCheckpointStore::connect(ValkeyConfig::new(vec![endpoint]))
            .await
            .expect("connect");
        let mut stale = store.partition_conn(1);
        let old_id: i64 = redis::cmd("CLIENT")
            .arg("ID")
            .query_async(&mut *stale)
            .await
            .expect("old client ID");
        let timeout = std::io::Error::new(std::io::ErrorKind::TimedOut, "test timeout");
        assert!(stale.resolve::<()>(Err(timeout.into())).await.is_err());
        let mut replacement = store.partition_conn(1);
        let new_id: i64 = redis::cmd("CLIENT")
            .arg("ID")
            .query_async(&mut *replacement)
            .await
            .expect("new client ID");
        assert_ne!(old_id, new_id);
        store
            .set_frontier(PartitionFrontier {
                partition: 1,
                sequence: 42,
            })
            .await
            .expect("new connection works");
        assert_eq!(
            store
                .frontier(1)
                .await
                .expect("frontier read")
                .unwrap()
                .sequence,
            42
        );
    }
}
