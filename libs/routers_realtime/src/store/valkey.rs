//! Valkey implementation of [`CheckpointStore`]. (T11)
//!
//! The durable home of every vehicle's committed checkpoint and in-flight
//! prepared commit, backed by a fleet of independent Valkey primaries. The
//! atomicity the trait promises is realised with per-vehicle Lua scripts run
//! over a single key group; the in-memory fake ([`super::checkpoint::
//! MemoryCheckpointStore`]) realises the same guarantees with one mutex.
//!
//! ## Placement and key groups
//!
//! A vehicle's three keys — its checkpoint, its prepared record, and (via the
//! partition index) its membership — must reach the same primary so a Lua
//! script can touch them in one critical section, and must reach the *same*
//! primary for the life of the vehicle so its history survives a fleet resize.
//! Both fall out of one decision: place by vehicle, with rendezvous
//! (highest-random-weight) hashing over the primaries' URLs, exactly as the
//! legacy raw-tail store does. Reordering or growing the fleet then remaps only
//! about `1/N` of vehicles rather than nearly all of them.
//!
//! The vehicle keys carry a `{vehicle:<id>}` hash tag so a future Valkey
//! Cluster hashes all three to one slot. The partition index
//! (`partition:<p>:prepared`) is deliberately *not* vehicle-tagged: it is placed
//! by the rendezvous of the string `partition:<p>`, so it can live on a
//! different primary than the vehicles it lists. That is safe because the index
//! is only a recovery hint — [`list_prepared`](CheckpointStore::list_prepared)
//! treats a listed-but-missing vehicle as stale and repairs it, and a leaked or
//! lost index entry is reconciled by recovery, never trusted as truth.
//!
//! ## Atomicity
//!
//! Compare-and-stage ([`prepare`](CheckpointStore::prepare)), promotion, and the
//! publish mark are single Lua scripts over the `{vehicle:<id>}` key group, so
//! each is atomic against concurrent commits for the same vehicle. The
//! `SADD`/`SREM` that maintain the partition index run as ordinary commands
//! after the script — they touch a different key group, cannot join the script's
//! atomic section, and are therefore best-effort by construction.
//!
//! No Valkey server exists in the build sandbox, so this module is
//! compile-verified: the unit tests cover key derivation, placement, the
//! stored-record hash encoding, and the Lua script argument layout.

use alloc::sync::Arc;
use core::str::FromStr;
use core::time::Duration;
use std::collections::HashMap;

use futures::future::try_join_all;
use redis::aio::MultiplexedConnection;
use thiserror::Error;
use url::Url;

use crate::event::VehicleId;
use crate::partition::{fnv1a, mix};
use crate::protocol::ids::{ObservationId, OutputId, Revision, SegmentId};
use crate::store::checkpoint::{
    CheckpointStore, CommitPhase, PartitionFrontier, PrepareOutcome, PreparedCommit,
    StoredCheckpoint,
};

/// The default idle lifetime of a committed checkpoint: ten minutes without a
/// commit and the vehicle's checkpoint may be reaped. Prepared records never
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
    /// [`ValkeyCheckpointStore::connect`] was handed an empty endpoint list; a
    /// fleet must have at least one primary to place a vehicle on.
    #[error("no valkey endpoints were supplied")]
    NoEndpoints,
    /// A promote or publish mark named an output that disagreed with the staged
    /// prepared record — a stale or crossed caller.
    #[error("prepared output mismatch: staged {staged}, got {got}")]
    OutputMismatch {
        /// The output the store has staged for the vehicle.
        staged: OutputId,
        /// The output the caller named.
        got: OutputId,
    },
    /// A stored hash held a field that was missing or could not be decoded back
    /// into its typed form.
    #[error("malformed stored record: {0}")]
    Malformed(String),
    /// A Lua script returned a status token this code does not recognise.
    #[error("unexpected script reply: {0:?}")]
    UnexpectedReply(Vec<String>),
}

/// Which primary owns a key. Held apart from the connections so the mapping —
/// the part that has to stay stable for history to survive — is testable
/// without a server. Copied from the legacy raw-tail store (`store/redis.rs`,
/// deleted in T33) so this module carries no dependency on it.
#[derive(Clone)]
struct Placement {
    /// One hash per endpoint URL. A primary's identity is its URL rather than
    /// its position, so reordering the fleet moves no vehicle.
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

    /// Rendezvous (highest-random-weight) placement. Plain modulo would remap
    /// nearly every vehicle when the fleet changes size, discarding the history
    /// that keeps trips continuous; this remaps about `1/N` of them.
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
/// tag, so all three of its keys place onto one primary (and, in a Cluster,
/// hash to one slot).
fn vehicle_slot(vehicle: VehicleId) -> String {
    format!("vehicle:{}", vehicle.0)
}

/// The rendezvous key of a partition's index and frontier. Not vehicle-tagged,
/// so the index may land on a different primary than the vehicles it lists.
fn partition_slot(partition: u16) -> String {
    format!("partition:{partition}")
}

/// The committed-checkpoint hash for a vehicle.
fn checkpoint_key(vehicle: VehicleId) -> String {
    format!("{{vehicle:{}}}:checkpoint", vehicle.0)
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

/// The checkpoint hash's fields (`revision`, `segment`, `bytes`) in the order
/// the store writes them. The revision and segment ride outside the opaque
/// `bytes` so [`prepare`](CheckpointStore::prepare) can compare a base without
/// decoding the sealed [`routers_network::Entry`] type.
///
/// The store never writes this hash from Rust — the `PROMOTE` script builds it
/// from the prepared record inside the vehicle's key group — so this helper
/// exists to mirror that layout and prove the [`stored_from_fields`] inverse
/// round-trips; it is test-only.
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

/// The prepared-commit hash's fields in the order the store writes them. The
/// [`prepare`](CheckpointStore::prepare) script reuses this order for its
/// `ARGV`, so the two cannot drift.
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

/// Compare-and-stage over a vehicle's `{vehicle:<id>}` key group.
///
/// `KEYS[1]` = checkpoint hash, `KEYS[2]` = prepared hash. `ARGV[1]` is the
/// expected base (`''` = no checkpoint may exist); `ARGV[2..=11]` are the
/// prepared record's fields in [`prepared_to_fields`] order.
const PREPARE_LUA: &str = r#"
local existing = redis.call('HGET', KEYS[2], 'output')
if existing then
  if existing == ARGV[2] then
    return {'already'}
  end
  return {'busy', existing}
end
local actual = redis.call('HGET', KEYS[1], 'revision')
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
///
/// `KEYS[1]` = checkpoint hash, `KEYS[2]` = prepared hash. `ARGV[1]` = the
/// output being promoted, `ARGV[2]` = the checkpoint TTL in milliseconds.
/// Idempotent when the prepared record is gone (`{'noop'}`); refuses a prepared
/// record for a different output (`{'mismatch', staged}`).
const PROMOTE_LUA: &str = r#"
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
redis.call('DEL', KEYS[2])
return {'ok'}
"#;

/// Mark the prepared record [`CommitPhase::Published`].
///
/// `KEYS[1]` = checkpoint hash (unused; passed so the call stays in the
/// vehicle's key group), `KEYS[2]` = prepared hash, `ARGV[1]` = the output.
/// Idempotent when the prepared record is gone (`{'noop'}`); refuses a different
/// output (`{'mismatch', staged}`).
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

/// The Lua scripts, compiled once and shared across every clone of the store.
struct Scripts {
    prepare: redis::Script,
    promote: redis::Script,
    mark_published: redis::Script,
}

impl Scripts {
    fn new() -> Self {
        Self {
            prepare: redis::Script::new(PREPARE_LUA),
            promote: redis::Script::new(PROMOTE_LUA),
            mark_published: redis::Script::new(MARK_PUBLISHED_LUA),
        }
    }
}

/// How to reach the Valkey fleet and how long a committed checkpoint lives.
#[derive(Clone, Debug)]
pub struct ValkeyConfig {
    /// The primaries, addressed by URL. A vehicle is placed onto exactly one of
    /// these by rendezvous hashing; the set may be reordered without moving any
    /// vehicle.
    pub urls: Vec<Url>,
    /// How long a committed checkpoint survives without a fresh commit, applied
    /// as a `PEXPIRE` on every promotion. Prepared records are never given a
    /// TTL — they are the durable record of an in-flight commit.
    pub checkpoint_ttl: Duration,
    /// How long to wait for each primary's connection to establish.
    pub connect_timeout: Duration,
}

impl ValkeyConfig {
    /// A config for `urls` with the default checkpoint TTL and connect timeout.
    pub fn new(urls: Vec<Url>) -> Self {
        Self {
            urls,
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
///
/// Cloning shares the underlying multiplexed sockets and the compiled scripts,
/// so a partition-worker pool clones one store rather than opening its own.
#[derive(Clone)]
pub struct ValkeyCheckpointStore {
    conns: Vec<MultiplexedConnection>,
    placement: Placement,
    scripts: Arc<Scripts>,
    cfg: ValkeyConfig,
}

impl ValkeyCheckpointStore {
    /// Open a multiplexed connection to every primary in `cfg`, concurrently.
    ///
    /// Errors if the endpoint list is empty (a fleet needs at least one primary
    /// to place a vehicle on) or if any connection cannot be established within
    /// [`ValkeyConfig::connect_timeout`].
    pub async fn connect(cfg: ValkeyConfig) -> Result<Self, ValkeyError> {
        if cfg.urls.is_empty() {
            return Err(ValkeyError::NoEndpoints);
        }

        let placement = Placement::new(&cfg.urls);

        // Concurrently: awaiting each primary in turn would multiply startup
        // latency by the size of the fleet.
        let conns = try_join_all(cfg.urls.iter().cloned().map(|url| {
            let connect_timeout = cfg.connect_timeout;
            async move {
                let client = redis::Client::open(url)?;
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

    /// A cheap clone of the connection to the primary that owns `vehicle`'s
    /// keys. A multiplexed connection clones by sharing its socket, so this does
    /// not open anything.
    fn vehicle_conn(&self, vehicle: VehicleId) -> MultiplexedConnection {
        self.conns[self.placement.index_for(&vehicle_slot(vehicle))].clone()
    }

    /// A cheap clone of the connection to the primary that owns `partition`'s
    /// index and frontier. May differ from the primary of a vehicle listed in
    /// that index (see the module docs).
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

impl CheckpointStore for ValkeyCheckpointStore {
    type Error = ValkeyError;

    async fn load(
        &self,
        vehicle: VehicleId,
    ) -> Result<(Option<StoredCheckpoint>, Option<PreparedCommit>), Self::Error> {
        let mut conn = self.vehicle_conn(vehicle);

        // Both keys are in the one key group, so a pipeline reads them on the
        // same connection round trip — a single point-in-time view.
        let mut pipe = redis::pipe();
        pipe.cmd("HGETALL")
            .arg(checkpoint_key(vehicle))
            .cmd("HGETALL")
            .arg(prepared_key(vehicle));
        let (checkpoint_fields, prepared_fields): (
            HashMap<String, Vec<u8>>,
            HashMap<String, Vec<u8>>,
        ) = pipe.query_async(&mut conn).await?;

        let checkpoint = if checkpoint_fields.is_empty() {
            None
        } else {
            Some(stored_from_fields(&checkpoint_fields)?)
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
            .arg(expected);
        for (_, value) in &fields {
            invocation.arg(value.as_slice());
        }
        let reply: Vec<String> = invocation.invoke_async(&mut conn).await?;
        let outcome = parse_prepare_reply(&reply)?;

        // Best-effort partition index. Add on both `Prepared` and
        // `AlreadyPrepared` so a retry that finds the record already staged
        // still repairs a missing index entry; `SADD` is idempotent. A failure
        // here surfaces as an error the idempotent retry can safely re-drive.
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
        // `as_millis` is u128; a TTL past `u64::MAX` ms is not reachable, so
        // saturate rather than wrap.
        let ttl_ms = u64::try_from(self.cfg.checkpoint_ttl.as_millis()).unwrap_or(u64::MAX);

        let mut conn = self.vehicle_conn(vehicle);
        let mut invocation = self.scripts.promote.prepare_invoke();
        invocation
            .key(checkpoint_key(vehicle))
            .key(prepared_key(vehicle))
            .arg(output.to_string())
            .arg(ttl_ms);
        let reply: Vec<String> = invocation.invoke_async(&mut conn).await?;
        parse_ok_or_mismatch(&reply, output)?;

        // Best-effort index removal. A leaked entry is harmless: `list_prepared`
        // finds the record gone and repairs it.
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
                // Promoted away since it was indexed: repair the stale hint.
                stale.push(vehicle);
            } else {
                prepared.push((vehicle, prepared_from_fields(&fields)?));
            }
        }

        // Repair stale index entries best-effort; a failure here is
        // reconciled on the next recovery pass and must not fail the listing.
        for vehicle in stale {
            let _: Result<i64, redis::RedisError> = redis::cmd("SREM")
                .arg(&index_key)
                .arg(vehicle.0)
                .query_async(&mut index_conn)
                .await;
        }

        // Deterministic order, independent of set iteration order.
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
        // Only the committed checkpoint is idle-expired; a prepared record is
        // the durable record of an in-flight commit and is never touched here.
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

    fn fleet(n: usize) -> Vec<Url> {
        (0..n)
            .map(|i| Url::parse(&format!("redis://valkey-{i:03}:6379")).unwrap())
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
        assert_eq!(prepared_key(vehicle), "{vehicle:42}:prepared");
        // Both vehicle keys share one `{...}` tag, so a Cluster hashes them to
        // one slot and this store places them on one primary.
        assert_eq!(vehicle_slot(vehicle), "vehicle:42");
    }

    #[test]
    fn partition_keys_are_not_vehicle_tagged() {
        assert_eq!(partition_index_key(7), "partition:7:prepared");
        assert_eq!(frontier_key(7), "partition:7:frontier");
        assert_eq!(partition_slot(7), "partition:7");
    }

    // --- placement (carried from the legacy store) --------------------------

    #[test]
    fn placement_is_deterministic() {
        let urls = fleet(20);
        let (a, b) = (Placement::new(&urls), Placement::new(&urls));
        for slot in slots(1000) {
            assert_eq!(a.index_for(&slot), b.index_for(&slot));
        }
    }

    /// The fleet is a set, not a list. Two binaries handed the same primaries in
    /// different orders must still agree on where a vehicle lives, or each would
    /// see only part of its history.
    #[test]
    fn placement_ignores_url_order() {
        let urls = fleet(20);
        let mut shuffled = urls.clone();
        shuffled.reverse();
        let (direct, reversed) = (Placement::new(&urls), Placement::new(&shuffled));
        for slot in slots(1000) {
            let expected = &urls[direct.index_for(&slot)];
            let actual = &shuffled[reversed.index_for(&slot)];
            assert_eq!(expected, actual, "{slot} moved when the list was reordered");
        }
    }

    /// Rendezvous hashing's reason for being. Modulo would move ~19/20 of the
    /// keyspace here; anything near that would discard the history that keeps
    /// trips continuous.
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
        // The vehicle keys place by `vehicle:<id>`; the partition index places
        // by `partition:<p>`. They may or may not coincide — the point is that
        // the vehicle placement never depends on the partition.
        let placement = Placement::new(&fleet(8));
        for id in 0..256u64 {
            let vehicle = VehicleId(id);
            // Deterministic and in range.
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
            // Binary, including bytes that are not valid UTF-8, to prove the
            // `bytes` field survives verbatim.
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
        // An empty actual means no checkpoint existed.
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

    // --- Lua argument-layout guards ----------------------------------------

    #[test]
    fn prepare_lua_references_its_keys_and_argv() {
        assert!(PREPARE_LUA.contains("KEYS[1]"));
        assert!(PREPARE_LUA.contains("KEYS[2]"));
        // ARGV[1] (expected base) through ARGV[11] (raw_sequence): one per
        // prepared field, plus the compare arg.
        for n in 1..=11 {
            assert!(
                PREPARE_LUA.contains(&format!("ARGV[{n}]")),
                "PREPARE is missing ARGV[{n}]"
            );
        }
        // ARGV[2..=11] line up with the prepared fields, in order.
        assert_eq!(
            prepared_to_fields(&sample_prepared(None, CommitPhase::Prepared)).len(),
            10
        );
        // The stored field names must appear verbatim in the HSET.
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
        assert!(PROMOTE_LUA.contains("ARGV[1]"));
        assert!(PROMOTE_LUA.contains("ARGV[2]"));
        // It installs the checkpoint from the prepared record and expires it.
        assert!(PROMOTE_LUA.contains("PEXPIRE"));
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
