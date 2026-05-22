//! Build-time ingestion filters.
//!
//! A [`ShardSource`](crate::ShardSource) is intentionally dumb: it yields
//! every node and every traversable way it finds. For a WASM bundle or a
//! constrained worker you usually want less than that — only the highway
//! class you care about, or topology *without* the metadata that drives
//! runtime decisions. [`IngestFilter`] lets you trim the data before it
//! lands in the [`ShardedNetwork`](crate::ShardedNetwork), so the on-disk
//! cache (and the in-memory graph) only ever contains what you'll use.
//!
//! The filter is consulted *per way*. Filtering at the source level (rather
//! than post-build) means the dropped nodes also fall out of the spatial
//! indices and never reach disk.

use core::fmt::Debug;
use routers_network::Metadata;

type WayPredicate<M> = Box<dyn Fn(&M) -> bool + Send + Sync>;

/// Configurable predicate + projection over the data ingested into a
/// [`ShardedNetwork`](crate::ShardedNetwork).
pub struct IngestFilter<M: Metadata> {
    way_predicate: Option<WayPredicate<M>>,
    strip_metadata: bool,
}

impl<M: Metadata + 'static> Default for IngestFilter<M> {
    fn default() -> Self {
        Self {
            way_predicate: None,
            strip_metadata: false,
        }
    }
}

impl<M: Metadata + 'static> Debug for IngestFilter<M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("IngestFilter")
            .field("way_predicate", &self.way_predicate.as_ref().map(|_| "<closure>"))
            .field("strip_metadata", &self.strip_metadata)
            .finish()
    }
}

impl<M: Metadata + 'static> IngestFilter<M> {
    /// An empty filter that keeps every way and retains all metadata.
    /// Equivalent to `Default::default()`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Drop any way whose metadata `pred` returns `false` for.
    ///
    /// Common uses:
    ///
    /// - `keep_ways_where(|m| m.road_class.map_or(false, |c| c >= RoadClass::Tertiary))`
    ///   strips local/residential streets, leaving only through-roads.
    /// - `keep_ways_where(|m| m.access.is_empty())` removes ways with any
    ///   access tag (typically private/forestry tracks).
    ///
    /// Predicates compose with `&&` if you call this method more than
    /// once — every previous predicate has to pass as well.
    pub fn keep_ways_where<F>(mut self, pred: F) -> Self
    where
        F: Fn(&M) -> bool + Send + Sync + 'static,
    {
        let new = Box::new(pred);
        self.way_predicate = match self.way_predicate.take() {
            None => Some(new),
            Some(existing) => Some(Box::new(move |m| existing(m) && new(m))),
        };
        self
    }

    /// Discard the per-way [`Metadata`] map entirely. The graph keeps the
    /// edge weights (so routing still works) but `Network::metadata`
    /// returns `None` for every way. Useful when you want a tiny bundle
    /// containing only topology + costs.
    pub fn without_metadata(mut self) -> Self {
        self.strip_metadata = true;
        self
    }

    #[inline]
    pub(crate) fn accepts(&self, m: &M) -> bool {
        self.way_predicate.as_ref().is_none_or(|p| p(m))
    }

    #[inline]
    pub(crate) fn keep_metadata(&self) -> bool {
        !self.strip_metadata
    }
}
