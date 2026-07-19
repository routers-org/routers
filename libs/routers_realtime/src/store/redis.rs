use std::collections::HashMap;

use redis::streams::StreamRangeReply;
use thiserror::Error;
use url::Url;

use crate::store::Storable;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("redis: {0}")]
    Redis(#[from] redis::RedisError),
    #[error("serialisation: {0}")]
    Serialisation(#[from] postcard::Error),
}

type Result<T> = std::result::Result<T, StoreError>;

pub struct RedisStore<T: Storable> {
    conn: redis::aio::MultiplexedConnection,
    _phantom: std::marker::PhantomData<T>,
}

impl<T: Storable> RedisStore<T> {
    pub async fn new(url: Url) -> Result<Self> {
        let client = redis::Client::open(url)?;
        let conn = client.get_multiplexed_async_connection().await?;

        Ok(Self {
            conn,
            _phantom: std::marker::PhantomData,
        })
    }
}

impl<T: Storable> RedisStore<T> {
    pub async fn get_many(&mut self, vehicle_id: &str, len: usize) -> Result<Vec<T>> {
        let key = format!("vehicle:{}:positions", vehicle_id);

        let reply: StreamRangeReply = redis::cmd("XREVRANGE")
            .arg(&key)
            .arg("+")
            .arg("-")
            .arg("COUNT")
            .arg(len)
            .query_async(&mut self.conn)
            .await?;

        let mut entries = Vec::with_capacity(reply.ids.len());

        for stream_id in &reply.ids {
            let value = match stream_id.map.get("val") {
                Some(redis::Value::BulkString(b)) => b.as_slice(),
                _ => continue,
            };

            let entry: T = postcard::from_bytes(value)?;
            entries.push(entry);
        }

        Ok(entries)
    }

    pub async fn write_many(&mut self, batch: &[T], limit: usize) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }

        let mut pipe = redis::pipe();

        for item in batch {
            let value = postcard::to_allocvec(item)?;
            let key = format!("vehicle:{}:positions", item.key());

            pipe.cmd("XADD")
                .arg(key)
                .arg("MAXLEN")
                .arg("~")
                .arg(limit)
                .arg("*")
                .arg("shard")
                .arg(item.shard_id().to_string())
                .arg("val")
                .arg(value)
                .ignore();
        }

        let _: () = pipe.query_async(&mut self.conn).await?;
        Ok(())
    }
}

pub struct CachedRedisStore<T: Storable> {
    store: RedisStore<T>,
    cache: HashMap<String, Vec<T>>,
}

impl<T: Storable> CachedRedisStore<T> {
    pub fn new(store: RedisStore<T>) -> Self {
        Self {
            store,
            cache: HashMap::new(),
        }
    }

    pub async fn get_many(&mut self, vehicle_id: &str, len: usize) -> Result<Vec<T>> {
        if let Some(cached) = self.cache.get(vehicle_id).cloned() {
            return Ok(cached);
        }

        let entries = self.store.get_many(vehicle_id, len).await?;
        self.cache.insert(vehicle_id.to_string(), entries.clone());
        Ok(entries)
    }

    /// Roll an observed item into the cached window (newest-first), so
    /// subsequent reads see it without re-querying the backing store. Without
    /// this the cache is a frozen snapshot of the first read — which, for a
    /// consumer racing the writer on a vehicle's first event, is empty
    /// forever.
    pub fn push(&mut self, key: &str, item: T, len: usize) {
        let entries = self.cache.entry(key.to_string()).or_default();
        entries.insert(0, item);
        entries.truncate(len);
    }

    pub async fn write_many(&mut self, batch: &[T], limit: usize) -> Result<()> {
        self.store.write_many(batch, limit).await
    }
}
