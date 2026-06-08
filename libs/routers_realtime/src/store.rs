use crate::context::Position;
use routers_shard::ShardId;
use serde::{Serialize, de::DeserializeOwned};
use std::collections::{HashMap, HashSet, VecDeque};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("serialization: {0}")]
    Serialize(#[from] postcard::Error),
}

pub trait PositionStore<S: ShardId> {
    fn push_and_fetch(
        &mut self,
        vehicle_id: &str,
        shard: S,
        position: Position,
    ) -> impl std::future::Future<Output = Result<Vec<(S, Position)>, StoreError>> + Send;
}

/// In-process position store — no network overhead, no persistence across restarts.
/// Useful for benchmarking to isolate pure CPU and NATS costs.
pub struct MemoryStore<S: ShardId> {
    inner: HashMap<String, VecDeque<(S, Position)>>,
    max_len: usize,
}

impl<S: ShardId> MemoryStore<S> {
    pub fn new(max_len: usize) -> Self {
        Self {
            inner: HashMap::new(),
            max_len,
        }
    }

    pub fn contains(&self, vehicle_id: &str) -> bool {
        self.inner.contains_key(vehicle_id)
    }

    /// Seed history for a vehicle from an external source (e.g. Valkey cold-start).
    /// Entries must be in newest-first order (XREVRANGE order). No-op if the vehicle
    /// is already present.
    pub fn seed(&mut self, vehicle_id: &str, history: Vec<(S, Position)>) {
        let entry = self.inner.entry(vehicle_id.to_string()).or_default();
        if entry.is_empty() {
            entry.extend(history.into_iter().take(self.max_len));
        }
    }
}

impl<S: ShardId + Clone + Send> PositionStore<S> for MemoryStore<S> {
    async fn push_and_fetch(
        &mut self,
        vehicle_id: &str,
        shard: S,
        position: Position,
    ) -> Result<Vec<(S, Position)>, StoreError> {
        let history = self.inner.entry(vehicle_id.to_string()).or_default();
        history.push_front((shard, position));
        if history.len() > self.max_len {
            history.pop_back();
        }
        Ok(history.iter().cloned().collect())
    }
}

pub struct ValkeyStore {
    conn: redis::aio::MultiplexedConnection,
    max_len: usize,
}

impl ValkeyStore {
    pub async fn connect(url: &str) -> Result<Self, redis::RedisError> {
        let client = redis::Client::open(url)?;
        let conn = client.get_multiplexed_async_connection().await?;
        Ok(Self { conn, max_len: 200 })
    }
}

impl<S> PositionStore<S> for ValkeyStore
where
    S: ShardId + Serialize + DeserializeOwned + Send,
{
    async fn push_and_fetch(
        &mut self,
        vehicle_id: &str,
        shard: S,
        position: Position,
    ) -> Result<Vec<(S, Position)>, StoreError> {
        let key = format!("vehicle:{}:positions", vehicle_id);
        let shard_bytes = postcard::to_allocvec(&shard)?;
        let pos_bytes = postcard::to_allocvec(&position)?;

        // Pipeline XADD + XREVRANGE in one round-trip rather than two sequential calls.
        let (_, reply): (redis::Value, redis::streams::StreamRangeReply) = redis::pipe()
            .cmd("XADD")
            .arg(&key)
            .arg("MAXLEN")
            .arg("~")
            .arg(self.max_len)
            .arg("*")
            .arg("shard")
            .arg(shard_bytes.as_slice())
            .arg("pos")
            .arg(pos_bytes.as_slice())
            .cmd("XREVRANGE")
            .arg(&key)
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(self.max_len)
            .query_async(&mut self.conn)
            .await?;

        decode_stream_reply::<S>(reply)
    }
}

impl ValkeyStore {
    /// XREVRANGE for a single vehicle — used for cold-start seeding of in-memory caches.
    /// Returns entries in newest-first order.
    pub async fn fetch_history<S>(
        &mut self,
        vehicle_id: &str,
    ) -> Result<Vec<(S, Position)>, StoreError>
    where
        S: ShardId + DeserializeOwned,
    {
        let key = format!("vehicle:{}:positions", vehicle_id);
        let reply: redis::streams::StreamRangeReply = redis::cmd("XREVRANGE")
            .arg(&key)
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(self.max_len)
            .query_async(&mut self.conn)
            .await?;
        decode_stream_reply::<S>(reply)
    }

    /// Pipeline N XADD + N XREVRANGE commands in one round-trip.
    /// Returns one history vec per input event, in the same order.
    pub async fn push_and_fetch_many<S>(
        &mut self,
        batch: &[(String, S, Position)],
    ) -> Result<Vec<Vec<(S, Position)>>, StoreError>
    where
        S: ShardId + Serialize + DeserializeOwned + Send,
    {
        if batch.is_empty() {
            return Ok(vec![]);
        }

        let mut serialized: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(batch.len());
        for (_, shard, position) in batch {
            serialized.push((
                postcard::to_allocvec(shard)?,
                postcard::to_allocvec(position)?,
            ));
        }

        let mut pipe = redis::pipe();
        // N XADDs — ignore reply (we only need the updated stream, not the new ID)
        for ((vehicle_id, _, _), (shard_bytes, pos_bytes)) in batch.iter().zip(serialized.iter()) {
            let key = format!("vehicle:{}:positions", vehicle_id);
            pipe.cmd("XADD")
                .arg(&key)
                .arg("MAXLEN")
                .arg("~")
                .arg(self.max_len)
                .arg("*")
                .arg("shard")
                .arg(shard_bytes.as_slice())
                .arg("pos")
                .arg(pos_bytes.as_slice())
                .ignore();
        }
        // N XREVRANGEs — results are collected
        for (vehicle_id, _, _) in batch {
            let key = format!("vehicle:{}:positions", vehicle_id);
            pipe.cmd("XREVRANGE")
                .arg(&key)
                .arg("+")
                .arg("-")
                .arg("COUNT")
                .arg(self.max_len);
        }

        let replies: Vec<redis::streams::StreamRangeReply> =
            pipe.query_async(&mut self.conn).await?;

        let mut results = Vec::with_capacity(batch.len());
        for reply in replies {
            results.push(decode_stream_reply(reply)?);
        }
        Ok(results)
    }

    /// Pipeline N XADD commands in one round-trip — no reads.
    /// Used by the historian binary to persist events without blocking the hot path.
    pub async fn write_many<S>(
        &mut self,
        batch: &[(String, S, Position)],
    ) -> Result<(), StoreError>
    where
        S: ShardId + Serialize + Send,
    {
        if batch.is_empty() {
            return Ok(());
        }
        let mut pipe = redis::pipe();
        for (vehicle_id, shard, position) in batch {
            let key = format!("vehicle:{}:positions", vehicle_id);
            let shard_bytes = postcard::to_allocvec(shard)?;
            let pos_bytes = postcard::to_allocvec(position)?;
            pipe.cmd("XADD")
                .arg(&key)
                .arg("MAXLEN")
                .arg("~")
                .arg(self.max_len)
                .arg("*")
                .arg("shard")
                .arg(shard_bytes.as_slice())
                .arg("pos")
                .arg(pos_bytes.as_slice())
                .ignore();
        }
        let _: () = pipe.query_async(&mut self.conn).await?;
        Ok(())
    }
}

/// In-memory hot path with lazy Valkey seeding on first access per vehicle.
///
/// The orchestrator reads entirely from memory after cold-start; a separate historian
/// process handles all Valkey writes asynchronously. On first event for an unknown
/// vehicle, one XREVRANGE seeds the in-memory store so the matcher has prior context
/// (e.g. after a pod restart or a new vehicle appearing mid-run).
pub struct WarmingMemoryStore<S: ShardId> {
    memory: MemoryStore<S>,
    valkey: ValkeyStore,
    seeded: HashSet<String>,
}

impl<S: ShardId> WarmingMemoryStore<S> {
    pub fn new(valkey: ValkeyStore, max_len: usize) -> Self {
        Self {
            memory: MemoryStore::new(max_len),
            valkey,
            seeded: HashSet::new(),
        }
    }
}

impl<S> PositionStore<S> for WarmingMemoryStore<S>
where
    S: ShardId + Clone + Send + Serialize + DeserializeOwned,
{
    async fn push_and_fetch(
        &mut self,
        vehicle_id: &str,
        shard: S,
        position: Position,
    ) -> Result<Vec<(S, Position)>, StoreError> {
        if !self.seeded.contains(vehicle_id) {
            let history = self.valkey.fetch_history::<S>(vehicle_id).await?;
            self.memory.seed(vehicle_id, history);
            self.seeded.insert(vehicle_id.to_string());
        }
        self.memory.push_and_fetch(vehicle_id, shard, position).await
    }
}

fn decode_stream_reply<S: ShardId + DeserializeOwned>(
    reply: redis::streams::StreamRangeReply,
) -> Result<Vec<(S, Position)>, StoreError> {
    let mut entries = Vec::with_capacity(reply.ids.len());
    for stream_id in &reply.ids {
        let shard_val = match stream_id.map.get("shard") {
            Some(redis::Value::BulkString(b)) => b.as_slice(),
            _ => continue,
        };
        let pos_val = match stream_id.map.get("pos") {
            Some(redis::Value::BulkString(b)) => b.as_slice(),
            _ => continue,
        };
        let s: S = postcard::from_bytes(shard_val)?;
        let p: Position = postcard::from_bytes(pos_val)?;
        entries.push((s, p));
    }
    Ok(entries)
}
