//! Asynchronous shard loading.
//!
//! A [`ShardLoader`] is the runtime counterpart to the build-time pipeline:
//! given a [`ShardId`] it locates the cached blob for that shard, decodes
//! it into a [`ShardedNetwork`], and keeps the result in a [`ShardCache`]
//! so subsequent lookups for the same shard are free.
//!
//! The trait surface is deliberately generic over the data types
//! (`<E, M, S>`) and over how blobs are *fetched* (the [`ShardFetcher`]
//! trait). Concrete fetcher implementations:
//!
//! - [`FileShardFetcher`] — reads a `.shard.rt` from the local filesystem
//!   (native-only)
//! - [`WebShardFetcher`] — fetches the same blob via `window.fetch` in the
//!   browser (wasm32-only)
//!
//! Implement [`ShardFetcher`] yourself to plug in any other transport
//! (S3, an in-memory test mock, a CDN with auth headers, …).

mod fetcher;
mod window;

pub use fetcher::ShardFetcher;
pub use window::{RecenterDelta, ShardWindow};

#[cfg(not(target_arch = "wasm32"))]
mod file;
#[cfg(not(target_arch = "wasm32"))]
pub use file::FileShardFetcher;

#[cfg(target_arch = "wasm32")]
mod web;
#[cfg(target_arch = "wasm32")]
pub use web::WebShardFetcher;

use core::fmt::Debug;
use log::debug;
use rustc_hash::FxHashMap;
use std::sync::Arc;

use routers_network::{Entry, Metadata};

use crate::network::ShardedNetwork;
use crate::strategy::ShardId;

/// In-memory map of loaded shards.
///
/// Networks are stored behind `Arc` so the cache can hand out cheap
/// references without copying the whole graph. Multiple consumers of the
/// same shard share a single backing allocation.
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

    /// Cloned `Arc` to the loaded shard, if present.
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

    /// Drop a shard from the cache. Useful for eviction policies built on
    /// top — the loader itself doesn't decide when to forget.
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
#[derive(Debug)]
pub enum LoadError<FetchErr> {
    /// The underlying fetcher failed (network error, missing file, etc.).
    Fetch(FetchErr),
    /// The fetched bytes were not a valid `ShardedNetwork` payload.
    Decode(String),
}

impl<F: core::fmt::Display> core::fmt::Display for LoadError<F> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LoadError::Fetch(e) => write!(f, "fetch failed: {e}"),
            LoadError::Decode(e) => write!(f, "decode failed: {e}"),
        }
    }
}

/// Combines a [`ShardFetcher`] with a [`ShardCache`] and a naming function
/// that maps a `ShardId` to the key the fetcher expects (e.g. a filename
/// or URL path segment).
///
/// The loader doesn't enforce a particular naming scheme — your build
/// pipeline picks one and the runtime uses the matching closure. Keeping
/// this pluggable means you can rotate filenames (versioning, hashing)
/// without changing the loader code.
pub struct ShardLoader<E, M, S, F, N>
where
    E: Entry,
    M: Metadata,
    S: ShardId,
    F: ShardFetcher,
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
    F: ShardFetcher,
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

    /// Look up `id` in the cache. Returns `None` if it hasn't been loaded
    /// yet — call [`load`](Self::load) to fetch it.
    pub fn get(&self, id: &S) -> Option<Arc<ShardedNetwork<E, M, S>>> {
        self.cache.get(id)
    }

    /// Borrow the cache for read-only iteration (e.g. "what shards do I
    /// currently have in memory?").
    pub fn cache(&self) -> &ShardCache<E, M, S> {
        &self.cache
    }

    /// Async-load `id` if it isn't already cached, then return a handle
    /// to the loaded shard.
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
        Ok(self.cache.get(id).expect("just inserted"))
    }

    /// Bulk variant: load every id from the iterator concurrently
    /// (sequentially on wasm — the browser is single-threaded anyway).
    /// Returns the first error encountered, leaving any already-cached
    /// shards intact.
    pub async fn load_many<I>(&mut self, ids: I) -> Result<(), LoadError<F::Error>>
    where
        I: IntoIterator<Item = S>,
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned,
        S: serde::de::DeserializeOwned,
    {
        for id in ids {
            self.load(&id).await?;
        }
        Ok(())
    }
}
