//! 9-cell sliding window of shards.
//!
//! Keeps the owned (centre) cell plus its 8 strategy-defined neighbours
//! warm in memory. As the consumer (typically a viewport) pans, calling
//! [`ShardWindow::recenter`] figures out which 9 cells should be live,
//! evicts the rest, and reports which ones need fetching — the caller
//! then drives the async fetch via [`ShardWindow::fetch_one`] using
//! whichever executor is appropriate (`wasm_bindgen_futures::spawn_local`
//! on the web, `tokio::task::spawn_local` natively).
//!
//! When the new centre is one of the previously-loaded neighbours, no
//! fetch happens — the cell is simply promoted. Cells that drop out of
//! the window are discarded and rely on HTTP caching (or whatever the
//! underlying `ShardFetcher` provides) for cheap re-fetches if the user
//! pans back.

use core::fmt::Debug;
use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::{Arc, Mutex};

use geo::Point;
use routers_network::{Entry, Metadata};

use super::LoadError;
use super::fetcher::Fetcher;
use crate::network::ShardedNetwork;
use crate::strategy::{ShardId, ShardingStrategy};

/// Snapshot of what changed when [`ShardWindow::recenter`] ran.
#[derive(Debug)]
pub struct RecenterDelta<S: ShardId> {
    /// The new centre cell. Always present — even if `unchanged` is true,
    /// callers may want the id for logging or to drive the viewer.
    pub center: S,
    /// `true` if the new centre equals the previous one and nothing was
    /// touched. Callers can short-circuit on this.
    pub unchanged: bool,
    /// Shards now in scope but not yet in the cache. Caller should
    /// fetch them.
    pub to_fetch: Vec<S>,
    /// Shards that were dropped from the cache because they fell out of
    /// the new window.
    pub evicted: Vec<S>,
}

#[derive(Debug)]
struct WindowState<E: Entry, M: Metadata, S: ShardId> {
    center: Option<S>,
    cache: FxHashMap<S, Arc<ShardedNetwork<E, M, S>>>,
}

/// The window itself. Cheap to clone — the cache and centre state sit
/// behind an `Arc<Mutex<_>>` so async fetches running on the executor
/// can write back without needing exclusive access.
pub struct ShardWindow<E, M, St, F>
where
    E: Entry,
    M: Metadata,
    St: ShardingStrategy,
    F: Fetcher,
{
    strategy: St,
    fetcher: F,
    naming: Arc<dyn Fn(&St::Id) -> String + Send + Sync>,
    state: Arc<Mutex<WindowState<E, M, St::Id>>>,
}

impl<E, M, St, F> Clone for ShardWindow<E, M, St, F>
where
    E: Entry,
    M: Metadata,
    St: ShardingStrategy + Clone,
    F: Fetcher + Clone,
{
    fn clone(&self) -> Self {
        Self {
            strategy: self.strategy.clone(),
            fetcher: self.fetcher.clone(),
            naming: self.naming.clone(),
            state: self.state.clone(),
        }
    }
}

impl<E, M, St, F> ShardWindow<E, M, St, F>
where
    E: Entry,
    M: Metadata,
    St: ShardingStrategy,
    F: Fetcher,
{
    /// Construct an empty window. No cells loaded yet — call
    /// [`recenter`](Self::recenter) followed by
    /// [`fetch_one`](Self::fetch_one) for each `to_fetch` key.
    pub fn new<N>(strategy: St, fetcher: F, naming: N) -> Self
    where
        N: Fn(&St::Id) -> String + Send + Sync + 'static,
    {
        Self {
            strategy,
            fetcher,
            naming: Arc::new(naming),
            state: Arc::new(Mutex::new(WindowState {
                center: None,
                cache: FxHashMap::default(),
            })),
        }
    }

    /// Reframe the window around `point`. Promotes a neighbour to centre
    /// when possible (no fetch), drops cells that fell out, and tells you
    /// which cells are now in scope but missing.
    pub fn recenter(&self, point: Point) -> RecenterDelta<St::Id> {
        let new_center = self.strategy.locate(point);

        let mut state = self.state.lock().expect("ShardWindow mutex poisoned");

        if state.center.as_ref() == Some(&new_center) {
            return RecenterDelta {
                center: new_center,
                unchanged: true,
                to_fetch: Vec::new(),
                evicted: Vec::new(),
            };
        }

        let mut window: FxHashSet<St::Id> = FxHashSet::default();
        window.insert(new_center.clone());
        for n in self.strategy.neighbours(&new_center) {
            window.insert(n);
        }

        // Drop cells outside the new window. Promotion (a neighbour
        // becoming the new centre) is implicit: anything still in
        // `window` is retained, the rest goes.
        let evicted: Vec<St::Id> = state
            .cache
            .keys()
            .filter(|k| !window.contains(k))
            .cloned()
            .collect();
        for k in &evicted {
            state.cache.remove(k);
        }

        let to_fetch: Vec<St::Id> = window
            .iter()
            .filter(|k| !state.cache.contains_key(k))
            .cloned()
            .collect();

        state.center = Some(new_center.clone());

        RecenterDelta {
            center: new_center,
            unchanged: false,
            to_fetch,
            evicted,
        }
    }

    /// Async-fetch and decode a single shard, inserting it into the
    /// cache on success. Safe to call concurrently for different keys.
    ///
    /// If `key` is no longer in the active window by the time the fetch
    /// completes (because the user panned away during the round-trip),
    /// the result is discarded — there's no point keeping a shard that
    /// the next [`recenter`](Self::recenter) call would evict anyway.
    pub async fn fetch_one(&self, key: &St::Id) -> Result<(), LoadError<F::Error>>
    where
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned,
        St::Id: serde::de::DeserializeOwned,
    {
        let name = (self.naming)(key);
        let bytes = self.fetcher.fetch(&name).await.map_err(LoadError::Fetch)?;
        let net =
            ShardedNetwork::<E, M, St::Id>::from_cached_bytes(&bytes).map_err(LoadError::Decode)?;

        // Recompute the window relative to the *current* centre. If a
        // pan happened during the fetch and we're now out of scope,
        // drop the result on the floor.
        let mut state = self.state.lock().expect("ShardWindow mutex poisoned");
        let still_relevant = match &state.center {
            Some(c) => c == key || self.strategy.neighbours(c).contains(key),
            None => false,
        };
        if still_relevant {
            state.cache.insert(key.clone(), Arc::new(net));
        }
        Ok(())
    }

    /// Cloned `Arc` to the current centre shard, if loaded. Returns
    /// `None` until the centre's fetch resolves.
    pub fn owned(&self) -> Option<Arc<ShardedNetwork<E, M, St::Id>>> {
        let state = self.state.lock().expect("ShardWindow mutex poisoned");
        let center = state.center.as_ref()?;
        state.cache.get(center).cloned()
    }

    /// The current centre id, regardless of whether it's loaded yet.
    pub fn center(&self) -> Option<St::Id> {
        self.state
            .lock()
            .expect("ShardWindow mutex poisoned")
            .center
            .clone()
    }

    /// Snapshot of every loaded shard's id. Useful for diagnostics or
    /// for a UI overlay showing "what's in memory right now".
    pub fn loaded_ids(&self) -> Vec<St::Id> {
        self.state
            .lock()
            .expect("ShardWindow mutex poisoned")
            .cache
            .keys()
            .cloned()
            .collect()
    }

    /// Cloned `Arc` to an arbitrary loaded shard by id.
    pub fn get(&self, id: &St::Id) -> Option<Arc<ShardedNetwork<E, M, St::Id>>> {
        self.state
            .lock()
            .expect("ShardWindow mutex poisoned")
            .cache
            .get(id)
            .cloned()
    }

    /// Reference to the partitioning strategy this window was built with.
    /// Useful for callers that want to compute neighbours, locate points,
    /// etc., without keeping their own copy.
    pub fn strategy(&self) -> &St {
        &self.strategy
    }
}
