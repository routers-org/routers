use alloc::sync::Arc;
use core::fmt::Debug;
use geo::Distance;
use routers_network::{DataPlane, Metadata, Network};
use rustc_hash::{FxBuildHasher, FxHashMap};
use scc::HashCache;
use scc::hash_cache::Entry;

/// Entries retained before the cache starts evicting.
///
/// A bound rather than a hint. The matcher is long-lived and every distinct
/// routing query inserts an entry, so an unbounded cache grows until the pod is
/// OOM-killed — which takes its shard offline until it reschedules and reloads
/// the shard file.
///
/// Keep this a power of two. [`HashCache`] rounds its maximum capacity up to
/// one, so 10,000 would silently become 16,384 and cost 64% more memory than
/// the number suggests.
pub const DEFAULT_CACHE_CAPACITY: usize = 8_192;

// Enforced here rather than in a test, because the cost of getting it wrong is
// silent: `HashCache` would round up and allocate more than the constant says.
const _: () = assert!(
    DEFAULT_CACHE_CAPACITY.is_power_of_two(),
    "DEFAULT_CACHE_CAPACITY must be a power of two, or HashCache rounds it up"
);

/// A generic read-through cache for a hashmap-backed data structure.
///
/// Backed by [`HashCache`], which is 32-way associative and evicts the least
/// recently used entry of a bucket once that bucket is full. Eviction is
/// therefore O(1) and bucket-local: the cache can evict a little before it is
/// globally full, which is the price of not scanning.
pub struct CacheMap<V, N, Meta>
where
    V: Debug,
    Meta: Debug,
    N: Network,
{
    pub(crate) map: HashCache<N::Entry, Arc<V>, FxBuildHasher>,
    pub(crate) metadata: Meta,
}

// Hand-rolled so the cache's own `Debug` — and its stats accessors — do not
// drag `V: Send + Sync + 'static` onto every use of `CacheMap`. The entries
// themselves are elided.
impl<V, N, Meta> Debug for CacheMap<V, N, Meta>
where
    V: Debug,
    Meta: Debug,
    N: Network,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CacheMap")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct LockedMap<V, N, Meta>(Arc<CacheMap<V, N, Meta>>)
where
    LockedMap<V, N, Meta>: Calculable<N, V>,
    N: Network,
    V: Debug,
    Meta: Debug;

impl<V, N, Meta> Default for LockedMap<V, N, Meta>
where
    LockedMap<V, N, Meta>: Calculable<N, V>,
    CacheMap<V, N, Meta>: Default,
    V: Debug,
    N: Network,
    Meta: Debug,
{
    fn default() -> Self {
        LockedMap(Arc::new(CacheMap::default()))
    }
}

impl<V, N, Meta> Clone for LockedMap<V, N, Meta>
where
    LockedMap<V, N, Meta>: Calculable<N, V>,
    CacheMap<V, N, Meta>: Default,
    N: Network,
    V: Debug,
    Meta: Debug,
{
    fn clone(&self) -> Self {
        LockedMap(Arc::clone(&self.0))
    }
}

impl<V, N, Meta> LockedMap<V, N, Meta>
where
    LockedMap<V, N, Meta>: Calculable<N, V>,
    N: Network,
    V: Debug + Send + Sync + 'static,
    Meta: Debug,
{
    /// Exposes a query call for the cache map, allowing the caller
    /// to use the cache in its intended read-through pattern design.
    ///
    /// ### Behaviour
    ///
    /// This function is only exposed for [`CacheMap`] implementations
    /// which implement [`Calculable`].
    ///
    /// The function returns the value, [`V`] wrapped in a reference counter.
    /// This, therefore does not require [`V`] to be `Clone`. However, it
    /// consumes an owned value of the key, [`N::Entry`], which is required
    /// for the call to the [`Calculable::calculate`] function.
    pub fn query(&self, ctx: &RoutingContext<N>, key: N::Entry) -> Arc<V> {
        if let Some(value) = self.0.map.read(&key, |_, v| Arc::clone(v)) {
            return value;
        }

        let calculated = Arc::new(self.calculate(ctx, key));
        let _ = self.0.map.put(key, Arc::clone(&calculated));

        calculated
    }
}

impl<V, N, Meta> CacheMap<V, N, Meta>
where
    V: Debug,
    N: Network,
    Meta: Debug,
{
    /// The only way to build one, so no construction site can quietly
    /// reintroduce an unbounded map — which is how the bound was lost before.
    pub(crate) fn with_metadata(metadata: Meta) -> Self {
        Self {
            map: HashCache::with_capacity_and_hasher(
                0,
                DEFAULT_CACHE_CAPACITY,
                FxBuildHasher::default(),
            ),
            metadata,
        }
    }
}

impl<V, N, Meta> Default for CacheMap<V, N, Meta>
where
    V: Debug + Send + Sync + 'static,
    N: Network,
    Meta: Default + Debug,
{
    fn default() -> Self {
        Self::with_metadata(Meta::default())
    }
}

/// Implementation of a routing-domain calculable KV pair.
///
/// Asserts that the value, [`V`] can be generated from the key
/// (the network's [`Entry`]), given routing context, and the base structure.
///
/// ### Examples
///
/// The [`SuccessorsCache`] and [`PredicateCache`] are both examples
/// of calculable elements.
///
/// The [`SuccessorsCache`], given an underlying map key,
/// can derive the successors using the routing map and an
/// upper-bounded dijkstra algorithm.
pub trait Calculable<N: Network, V> {
    /// The concrete implementation of the function which derives the
    /// value, [`V`], from the key.
    ///
    /// The function parameters include relevant [`RoutingContext`] which
    /// may be required for the calculation.
    fn calculate(&self, ctx: &RoutingContext<N>, key: N::Entry) -> V;
}

mod successor {
    use super::*;
    use crate::primitives::WeightAndDistance;

    use geo::Haversine;
    use routers_network::DirectionAwareEdgeId;

    /// The weights, given as output from the [`SuccessorsCache::calculate`] function.
    type SuccessorWeights<E> = Vec<(E, DirectionAwareEdgeId<E>, WeightAndDistance)>;

    /// Per-node cache of the **accessible** outgoing edges and their costs.
    ///
    /// Resolves two things once per node and caches the result: the geometry
    /// (distance/weight to each neighbour) *and* whether each edge is traversable
    /// under the routing runtime. The reachability Dijkstra then walks this list
    /// directly — where it used to call `metadata`/`accessible` on every edge of
    /// every relaxation, the accessibility check now happens once per node.
    ///
    /// Because the accessibility filter is runtime-dependent, a cached list is
    /// only valid for one runtime; the enclosing [`PredicateCache`] binds and
    /// guards that runtime, and this cache is only reached through it. (The
    /// geometry half is runtime-independent, but keeping one cache avoids a
    /// second lookup + allocation per relaxation, which measured *slower* than
    /// the per-edge check it would replace.)
    pub type SuccessorsCache<N> = LockedMap<SuccessorWeights<<N as DataPlane>::Entry>, N, ()>;

    impl<N: Network> Calculable<N, SuccessorWeights<N::Entry>> for SuccessorsCache<N> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<N>, key: N::Entry) -> SuccessorWeights<N::Entry> {
            // Calc. once
            #[allow(unsafe_code)]
            let source = unsafe { ctx.map.point(&key).unwrap_unchecked() };

            ctx.map
                .edges_outof(key)
                // Keep only edges traversable by the runtime; an edge with no
                // metadata is not traversable. Resolved here, once per node,
                // rather than on every Dijkstra relaxation.
                .filter(|(_, _, (_, edge))| match ctx.map.metadata(&edge.index()) {
                    Some(meta) => meta.accessible(ctx.runtime, edge.direction()),
                    None => false,
                })
                .map(|(_, next, (w, edge))| {
                    const METER_TO_CM: f64 = 100.0;

                    #[allow(unsafe_code)]
                    let position = unsafe { ctx.map.point(&next).unwrap_unchecked() };

                    // In centimeters (1m = 100cm)
                    let distance = Haversine.distance(source, position);
                    (next, (distance * METER_TO_CM) as u32, w, edge)
                })
                .map(|(next, distance, weight, edge)| {
                    // Stores the weight and distance (in cm) to the candidate
                    let cost = WeightAndDistance::new(weight, distance);

                    (next, edge, cost)
                })
                .collect::<Vec<_>>()
        }
    }
}

mod predicate {
    use std::sync::OnceLock;

    use crate::primitives::{Dijkstra, algorithms::DijkstraReachableItem};
    use routers_network::Network;

    use super::*;

    const DEFAULT_THRESHOLD: f64 = 200_000f64; // 2km in cm

    #[derive(Debug)]
    pub struct PredicateMetadata<N>
    where
        N: Network,
    {
        /// Accessibility-resolved successors: resolves each node's traversable
        /// edges (and their costs) once, so the reachability Dijkstra no longer
        /// re-checks accessibility on every edge relaxation.
        successors: SuccessorsCache<N>,

        /// The threshold by which the solver is bounded, in centimeters.
        threshold_distance: f64,

        /// The runtime this cache was first queried with. Every stored
        /// reachability map is filtered for it, so serving another runtime
        /// would return one configuration's reachability for another's query —
        /// the tainting this cache is now guarded against. Captured lazily on
        /// the first query so callers still construct the cache without a
        /// runtime (which only arrives per-match).
        bound_runtime: OnceLock<N::Runtime>,
    }

    impl<N> Default for PredicateMetadata<N>
    where
        N: Network,
    {
        fn default() -> Self {
            Self {
                successors: SuccessorsCache::default(),
                threshold_distance: DEFAULT_THRESHOLD,
                bound_runtime: OnceLock::new(),
            }
        }
    }

    /// Predicates represents a hashmap of the input [`NodeIx`] as the key,
    /// mapped to the parent [`NodeIx`] it was reached from during an
    /// upper-bounded dijkstra calculation. Following the parent pointers back
    /// to the root reconstructs the path to any reachable node.
    ///
    /// The output from the [`PredicateCache::calculate`] function.
    type Predicates<E> = FxHashMap<E, E>;

    /// The reachability cache a weigher answers routing queries from.
    ///
    /// Keyed by a root node, it holds the parent-pointer map of an
    /// upper-bounded Dijkstra rooted there: every node reachable within the
    /// threshold, mapped to the node it was reached from. Computed once on
    /// first query and read thereafter — and deterministic, which is what
    /// lets collapse re-derive hop geometry rather than store it.
    ///
    /// Matching many trajectories over the same map? Share one cache across
    /// matches (see
    /// [`MatchOptions::with_cache`](crate::MatchOptions::with_cache)) so
    /// later matches run warm.
    pub type PredicateCache<N> =
        LockedMap<Predicates<<N as DataPlane>::Entry>, N, PredicateMetadata<N>>;

    impl<N: Network> PredicateCache<N> {
        pub fn with_threshold(threshold_cm: f64) -> Self {
            LockedMap(Arc::new(CacheMap::with_metadata(PredicateMetadata {
                successors: SuccessorsCache::default(),
                threshold_distance: threshold_cm,
                bound_runtime: OnceLock::new(),
            })))
        }
    }

    impl<N: Network> Calculable<N, Predicates<N::Entry>> for PredicateCache<N> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<N>, key: N::Entry) -> Predicates<N::Entry> {
            // Bind this cache to the runtime it first serves, and guard that it
            // is never reused with another. Accessibility — and therefore every
            // reachability map stored here — is a function of the runtime, so
            // mixing runtimes on one cache would hand a query the wrong
            // configuration's reachability. This is the invariant the old
            // inline-filter TODO could not uphold; resolving accessibility
            // through a runtime-bound cache does.
            let bound = self
                .0
                .metadata
                .bound_runtime
                .get_or_init(|| ctx.runtime.clone());
            // A hard invariant, not a debug check: serving a different runtime
            // would return one configuration's reachability for another's query
            // — silently wrong routes. The compare is one cheap `PartialEq` per
            // cache miss (each of which is already a full Dijkstra), so enforce
            // it in every build. In practice a cache serves one runtime (one
            // mode per shard/deployment); multiple modes need a cache each.
            assert!(
                bound == ctx.runtime,
                "PredicateCache reused across runtimes: its cached reachability was \
                 resolved for a different routing configuration and would taint this \
                 query — use a separate cache per runtime"
            );

            let threshold = self.0.metadata.threshold_distance;

            Dijkstra
                .reach(&key, move |node| {
                    // Successors are already accessibility-resolved for the
                    // bound runtime, so the hot relaxation loop is a plain walk.
                    ArcIter::new(self.0.metadata.successors.query(ctx, *node))
                        .map(|(a, _, b)| (a, b))
                })
                .take_while(|p| {
                    // Bounded by the threshold distance (centimeters)
                    (p.total_cost.distance_cm() as f64) < threshold
                })
                .map(|DijkstraReachableItem { node, parent, .. }| {
                    (node, parent.unwrap_or_default())
                })
                .collect::<Predicates<N::Entry>>()
        }
    }
}

/// Iterator wrapper that keeps the Arc alive while yielding `&T`
struct ArcIter<T> {
    data: Arc<Vec<T>>,
    index: usize,
}

impl<T> ArcIter<T> {
    #[inline(always)]
    fn new(data: Arc<Vec<T>>) -> Self {
        ArcIter { data, index: 0 }
    }
}

impl<T> Iterator for ArcIter<T>
where
    T: Copy,
{
    type Item = T;

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        let item = *self.data.get(self.index)?;
        self.index += 1;
        Some(item)
    }
}

pub use predicate::PredicateCache;
pub use successor::SuccessorsCache;

use crate::primitives::RoutingContext;

#[cfg(test)]
mod tests {
    use super::{Arc, DEFAULT_CACHE_CAPACITY, PredicateCache};
    use routers_network::mock::{MockEntryId, MockNetwork};

    /// Got caught out by nodes exceeding their memory limit,
    /// so we must ensure the cache size stays bounded.
    #[test]
    fn the_cache_stops_growing() {
        let cache = PredicateCache::<MockNetwork>::default();

        for key in 0..(DEFAULT_CACHE_CAPACITY as i64 * 10) {
            let _ = cache.0.map.put(MockEntryId(key), Arc::default());
        }

        assert_eq!(
            cache.0.map.len(),
            DEFAULT_CACHE_CAPACITY,
            "cache should sit at its bound after 10x that many inserts"
        );

        // The requested capacity has to be the one enforced. A
        // non-power-of-two constant would be rounded up and cost more memory
        // than it advertises; `DEFAULT_CACHE_CAPACITY` has a const assert for
        // that, and this confirms the constructor honours it.
        assert_eq!(cache.0.map.capacity(), DEFAULT_CACHE_CAPACITY);
    }
}
