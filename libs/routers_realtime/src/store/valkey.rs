//! Valkey implementation of [`CheckpointStore`], backed by a fleet of
//! independent primaries.
//!
//! A vehicle's keys carry a `{vehicle:<id>}` hash tag and are placed by
//! rendezvous hashing, so all reach one primary and a resize remaps only ~`1/N`.
//! Per-vehicle atomicity comes from Lua scripts over that key group; the
//! `SADD`/`SREM` partition-index writes run afterwards and are best-effort, a
//! recovery hint that [`list_prepared`](CheckpointStore::list_prepared) repairs.

use alloc::sync::Arc;
use core::str::FromStr;
use core::time::Duration;
use std::collections::HashMap;

use futures::future::try_join_all;
use redis::aio::MultiplexedConnection;
use thiserror::Error;

use crate::event::VehicleId;
use crate::partition::{fnv1a, mix};
use crate::protocol::ids::{ObservationId, OutputId, Revision, SegmentId, token_safe};
use crate::secret::SecretUrl;
use crate::store::checkpoint::{
    CheckpointStore, CommitPhase, PartitionFrontier, PrepareOutcome, PreparedCommit,
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

/// The Lua scripts, compiled once and shared across every clone of the store.
struct Scripts {
    prepare: redis::Script,
    promote: redis::Script,
    mark_published: redis::Script,
    finish_published: redis::Script,
}

impl Scripts {
    fn new() -> Self {
        Self {
            prepare: redis::Script::new(PREPARE_LUA),
            promote: redis::Script::new(PROMOTE_LUA),
            mark_published: redis::Script::new(MARK_PUBLISHED_LUA),
            finish_published: redis::Script::new(FINISH_PUBLISHED_LUA),
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
}

impl ValkeyConfig {
    /// A config for `urls` with the default checkpoint TTL and connect timeout.
    pub fn new(endpoints: Vec<ValkeyEndpoint>) -> Self {
        Self {
            endpoints,
            checkpoint_ttl: DEFAULT_CHECKPOINT_TTL,
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
        }
    }
}

impl Default for ValkeyConfig {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

/// A [`CheckpointStore`] backed by a fleet of independent Valkey primaries.
/// Cloning shares the multiplexed sockets and compiled scripts.
#[derive(Clone)]
pub struct ValkeyCheckpointStore {
    conns: Vec<MultiplexedConnection>,
    placement: Placement,
    scripts: Arc<Scripts>,
    cfg: ValkeyConfig,
}

impl ValkeyCheckpointStore {
    /// Open a multiplexed connection to every primary in `cfg`, concurrently.
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

        let conns = try_join_all(cfg.endpoints.iter().cloned().map(|endpoint| {
            let connect_timeout = cfg.connect_timeout;
            async move {
                let client = redis::Client::open(endpoint.url.connection_url())?;
                let config =
                    redis::AsyncConnectionConfig::new().set_connection_timeout(connect_timeout);
                let conn = client
                    .get_multiplexed_async_connection_with_config(&config)
                    .await?;
                Ok::<_, ValkeyError>(conn)
            }
        }))
        .await?;

        Ok(Self {
            conns,
            placement,
            scripts: Arc::new(Scripts::new()),
            cfg,
        })
    }

    /// The connection to the primary that owns `vehicle`'s keys; a cheap clone
    /// sharing the socket.
    fn vehicle_conn(&self, vehicle: VehicleId) -> MultiplexedConnection {
        self.conns[self.placement.index_for(&vehicle_slot(vehicle))].clone()
    }

    /// The connection to the primary that owns `partition`'s index and frontier;
    /// may differ from a listed vehicle's primary.
    fn partition_conn(&self, partition: u16) -> MultiplexedConnection {
        self.conns[self.placement.index_for(&partition_slot(partition))].clone()
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
        let (checkpoint_fields, committed_revision, prepared_fields): LoadReply =
            pipe.query_async(&mut conn).await?;

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
        let reply: Vec<String> = invocation.invoke_async(&mut conn).await?;
        let outcome = parse_prepare_reply(&reply)?;

        // Best-effort index: add on `AlreadyPrepared` too so a retry repairs a
        // missing entry (`SADD` is idempotent).
        if matches!(
            outcome,
            PrepareOutcome::Prepared | PrepareOutcome::AlreadyPrepared
        ) {
            let mut index_conn = self.partition_conn(partition);
            redis::cmd("SADD")
                .arg(partition_index_key(partition))
                .arg(vehicle.0)
                .query_async::<i64>(&mut index_conn)
                .await?;
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
        let reply: Vec<String> = invocation.invoke_async(&mut conn).await?;
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
        let reply: Vec<String> = invocation.invoke_async(&mut conn).await?;
        parse_promote_reply(&reply, output)?;

        // Best-effort index removal; a leaked entry is repaired by `list_prepared`.
        let mut index_conn = self.partition_conn(partition);
        redis::cmd("SREM")
            .arg(partition_index_key(partition))
            .arg(vehicle.0)
            .query_async::<i64>(&mut index_conn)
            .await?;
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
        let reply: Vec<String> = invocation.invoke_async(&mut conn).await?;
        parse_ok_or_mismatch(&reply, output)?;

        let mut index_conn = self.partition_conn(partition);
        redis::cmd("SREM")
            .arg(partition_index_key(partition))
            .arg(vehicle.0)
            .query_async::<i64>(&mut index_conn)
            .await?;
        Ok(())
    }

    async fn list_prepared(
        &self,
        partition: u16,
    ) -> Result<Vec<(VehicleId, PreparedCommit)>, Self::Error> {
        let index_key = partition_index_key(partition);
        let mut index_conn = self.partition_conn(partition);
        let ids: Vec<u64> = redis::cmd("SMEMBERS")
            .arg(&index_key)
            .query_async(&mut index_conn)
            .await?;

        let mut prepared = Vec::with_capacity(ids.len());
        let mut stale = Vec::new();
        for id in ids {
            let vehicle = VehicleId(id);
            let mut conn = self.vehicle_conn(vehicle);
            let fields: HashMap<String, Vec<u8>> = redis::cmd("HGETALL")
                .arg(prepared_key(vehicle))
                .query_async(&mut conn)
                .await?;
            if fields.is_empty() {
                stale.push(vehicle);
            } else {
                prepared.push((vehicle, prepared_from_fields(&fields)?));
            }
        }

        // Best-effort repair; a failure is reconciled on the next recovery pass.
        for vehicle in stale {
            let _: Result<i64, redis::RedisError> = redis::cmd("SREM")
                .arg(&index_key)
                .arg(vehicle.0)
                .query_async(&mut index_conn)
                .await;
        }

        // Deterministic order, independent of set iteration.
        prepared.sort_by_key(|(vehicle, _)| vehicle.0);
        Ok(prepared)
    }

    async fn frontier(&self, partition: u16) -> Result<Option<PartitionFrontier>, Self::Error> {
        let mut conn = self.partition_conn(partition);
        let sequence: Option<u64> = redis::cmd("GET")
            .arg(frontier_key(partition))
            .query_async(&mut conn)
            .await?;
        Ok(sequence.map(|sequence| PartitionFrontier {
            partition,
            sequence,
        }))
    }

    async fn set_frontier(&self, frontier: PartitionFrontier) -> Result<(), Self::Error> {
        let mut conn = self.partition_conn(frontier.partition);
        redis::cmd("SET")
            .arg(frontier_key(frontier.partition))
            .arg(frontier.sequence)
            .query_async::<()>(&mut conn)
            .await?;
        Ok(())
    }

    async fn expire_idle(&self, vehicle: VehicleId) -> Result<(), Self::Error> {
        // Never touch a prepared record: an in-flight commit must survive.
        let mut conn = self.vehicle_conn(vehicle);
        redis::cmd("DEL")
            .arg(checkpoint_key(vehicle))
            .query_async::<i64>(&mut conn)
            .await?;
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
    }
}
