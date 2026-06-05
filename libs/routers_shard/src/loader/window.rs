//! A 9-cell sliding window of shards.
//!
//! The cells are structured around a central, "owned" cell. This is
//! surrounded by 8 neighbour cells that are not the intended target of
//! requests, but permits edge-case handling by understanding incoming
//! and outgoing network shards.
//!

use core::fmt::Debug;
use rustc_hash::FxHashMap;
use std::sync::Arc;

use geo::Point;
use routers_network::{Entry, Metadata};

use super::LoadError;
use super::fetcher::Fetcher;
use crate::network::ShardedNetwork;
use crate::strategy::{ShardId, ShardingStrategy};

#[derive(Debug, Clone)]
pub enum ShardMoveDelta<S: ShardId> {
    Recentered {
        /// Shards now in scope, but not yet in the cache.
        scoped: Vec<S>,

        /// Shards that were in the cache but are no longer in scope.
        /// These will be evicted from the cache when the window moves, or kept.
        ///
        /// Note that keeping elements in the cache will increase memory overhead,
        /// but will reduce latency on consecutive requests for the same shard.
        unscoped: Vec<S>,
    },
    Unchanged,
}

#[derive(Debug)]
struct State<E: Entry, M: Metadata, S: ShardId> {
    /// The shard center, if allocated.
    center: Option<S>,

    /// The cache of loaded shards.
    cache: FxHashMap<S, Arc<ShardedNetwork<E, M, S>>>,
}

impl<E: Entry, M: Metadata, S: ShardId> Default for State<E, M, S> {
    fn default() -> Self {
        Self {
            center: None,
            cache: FxHashMap::default(),
        }
    }
}

pub struct ShardWindow<E, M, S, F>
where
    E: Entry,
    M: Metadata,
    S: ShardingStrategy,
    F: Fetcher,
{
    strategy: S,
    fetcher: F,
    state: State<E, M, S::Id>,
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
    /// [`fetch`](Self::fetch) for each key in the returned `scoped` list.
    pub fn new(strategy: St, fetcher: F) -> Self {
        Self {
            strategy,
            fetcher,
            state: State::default(),
        }
    }

    /// Reframe the window around `point`, the returned delta informs the caller
    /// which cells are now in scope but missing, and which cells are no longer
    /// in scope and can be evicted.
    ///
    /// This function does not modify the cache.
    pub fn recenter(&mut self, point: Point) -> ShardMoveDelta<St::Id> {
        let candidate_center = self.strategy.locate(point);

        match self.state.center {
            Some(center) if center.eq(&candidate_center) => ShardMoveDelta::Unchanged,
            _ => {
                let neighbours = self.strategy.neighbours(&candidate_center);
                let existing = self.loaded_ids();

                let all_ids = [&neighbours[..], &existing[..], &[candidate_center]].concat();
                let (unscoped, scoped): (Vec<St::Id>, Vec<St::Id>) = all_ids
                    .into_iter()
                    .partition(|k| !neighbours.contains(k) && *k != candidate_center);

                self.state.center = Some(candidate_center);
                ShardMoveDelta::Recentered { scoped, unscoped }
            }
        }
    }

    pub fn evict(&mut self, key: &St::Id) {
        self.state.cache.remove(key);
    }

    /// Fetch and decode a single shard, inserting it into the cache.
    pub async fn fetch(
        &mut self,
        key: &St::Id,
    ) -> Result<Arc<ShardedNetwork<E, M, St::Id>>, LoadError<F::Error>>
    where
        E: serde::de::DeserializeOwned,
        M: serde::de::DeserializeOwned,
        St::Id: serde::de::DeserializeOwned,
    {
        let name = key.to_string();
        let bytes = self.fetcher.fetch(&name).await.map_err(LoadError::Fetch)?;

        let net =
            ShardedNetwork::<E, M, St::Id>::from_cached_bytes(&bytes).map_err(LoadError::Decode)?;

        let refcounted = Arc::new(net);
        self.state.cache.insert(key.clone(), refcounted.clone());

        Ok(refcounted)
    }

    /// The current centre id, regardless of whether it's loaded yet.
    pub fn center(&self) -> Option<St::Id> {
        self.state.center
    }

    /// Snapshot of every loaded shard's id. Useful for diagnostics or
    /// for a UI overlay showing "what's in memory right now".
    pub fn loaded_ids(&self) -> Vec<St::Id> {
        self.state.cache.keys().copied().collect()
    }

    /// Cloned `Arc` to an arbitrary loaded shard by id.
    pub fn get(&self, id: &St::Id) -> Option<Arc<ShardedNetwork<E, M, St::Id>>> {
        self.state.cache.get(id).cloned()
    }

    /// Reference to the partitioning strategy this window was built with.
    /// Useful for callers that want to compute neighbours, locate points,
    /// etc., without keeping their own copy.
    pub fn strategy(&self) -> &St {
        &self.strategy
    }
}
