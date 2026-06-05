//! A [`ShardLoader`] is the runtime counterpart to the build-time pipeline:
//! given a [`ShardId`] it locates the cached blob for that shard, decodes
//! it into a [`ShardedNetwork`], and keeps the result in a [`ShardCache`]
//! so subsequent lookups for the same shard are free.
//!
//! - [`FileShardFetcher`] reads a `.shard.rt` from the local filesystem.
//! - [`WebFetcher`] fetches the shard via `window.fetch` (from the browser).
//!

use thiserror::Error;

mod fetcher;
mod window;

pub use fetcher::Fetcher;
pub use window::{RecenterDelta, ShardWindow};

#[cfg(not(target_arch = "wasm32"))]
mod file;
#[cfg(not(target_arch = "wasm32"))]
pub use file::FileFetcher;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::WebFetcher;

use core::fmt::Debug;
use log::debug;
use rustc_hash::FxHashMap;
use std::sync::Arc;

use routers_network::{Entry, Metadata};

use crate::network::ShardedNetwork;
use crate::strategy::ShardId;

/// In-memory map of loaded shards.
#[derive(Debug)]
pub struct ShardCache<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    map: FxHashMap<S, Arc<ShardedNetwork<E, M, S>>>,
}

impl<E, M, S> Default for ShardCache<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    fn default() -> Self {
        Self {
            map: FxHashMap::default(),
        }
    }
}

impl<E, M, S> ShardCache<E, M, S>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Is this shard already loaded?
    #[inline]
    pub fn contains(&self, id: &S) -> bool {
        self.map.contains_key(id)
    }

    pub fn get(&self, id: &S) -> Option<Arc<ShardedNetwork<E, M, S>>> {
        self.map.get(id).cloned()
    }

    /// Insert a freshly-loaded shard. Returns the previous value if any.
    pub fn insert(
        &mut self,
        id: S,
        net: ShardedNetwork<E, M, S>,
    ) -> Option<Arc<ShardedNetwork<E, M, S>>> {
        self.map.insert(id, Arc::new(net))
    }

    /// Drop a shard from the cache, by it's [`ShardId`].
    pub fn evict(&mut self, id: &S) -> Option<Arc<ShardedNetwork<E, M, S>>> {
        self.map.remove(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    pub fn loaded_ids(&self) -> impl Iterator<Item = &S> {
        self.map.keys()
    }
}

/// Errors a [`ShardLoader`] can surface.
#[derive(Error, Debug)]
pub enum LoadError<FetchErr> {
    #[error("fetch failed: {0}")]
    /// The underlying fetcher failed (network error, missing file, etc.).
    Fetch(FetchErr),

    #[error("decode failed: {0}")]
    /// The fetched bytes were not a valid `ShardedNetwork` payload.
    Decode(String),
}

pub struct ShardLoader<E, M, S, F, N>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
    F: Fetcher,
    N: Fn(&S) -> String,
{
    fetcher: F,
    naming: N,
    cache: ShardCache<E, M, S>,
}

impl<E, M, S, F, N> ShardLoader<E, M, S, F, N>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
    F: Fetcher,
    N: Fn(&S) -> String,
{
    pub fn new(fetcher: F, naming: N) -> Self {
        Self {
            fetcher,
            naming,
            cache: ShardCache::new(),
        }
    }

    pub fn with_cache(fetcher: F, naming: N, cache: ShardCache<E, M, S>) -> Self {
        Self {
            fetcher,
            naming,
            cache,
        }
    }

    /// Look up `id` in the cache.
    pub fn get(&self, id: &S) -> Option<Arc<ShardedNetwork<E, M, S>>> {
        self.cache.get(id)
    }

    /// Borrow the cache for read-only use.
    pub fn cache(&self) -> &ShardCache<E, M, S> {
        &self.cache
    }

    /// Load a shard. If already loaded, will return early.
    pub async fn load(
        &mut self,
        id: &S,
    ) -> Result<Arc<ShardedNetwork<E, M, S>>, LoadError<F::Error>>
    where
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned,
        S: serde::de::DeserializeOwned,
    {
        if let Some(net) = self.cache.get(id) {
            return Ok(net);
        }

        let key = (self.naming)(id);
        debug!("ShardLoader fetching {key}");

        let bytes = self.fetcher.fetch(&key).await.map_err(LoadError::Fetch)?;
        let net =
            ShardedNetwork::<E, M, S>::from_cached_bytes(&bytes).map_err(LoadError::Decode)?;

        self.cache.insert(id.clone(), net);
        let shard = self.cache.get(id).expect("must have inserted");

        Ok(shard)
    }
}
