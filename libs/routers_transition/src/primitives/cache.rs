use alloc::sync::Arc;
use core::fmt::Debug;
use geo::Distance;
use routers_network::{DataPlane, Metadata, Network};
use rustc_hash::{FxBuildHasher, FxHashMap};

use backing::Backing;

// Compatability layer for WASM
mod backing {
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) type Backing<K, V> = scc::HashCache<K, V, rustc_hash::FxBuildHasher>;

    #[cfg(target_arch = "wasm32")]
    pub(crate) struct Backing<K, V> {
        map: std::sync::Mutex<rustc_hash::FxHashMap<K, V>>,
        capacity: usize,
    }

    #[cfg(target_arch = "wasm32")]
    impl<K: core::hash::Hash + Eq, V> Backing<K, V> {
        pub(crate) fn with_capacity_and_hasher(
            _minimum: usize,
            capacity: usize,
            hasher: rustc_hash::FxBuildHasher,
        ) -> Self {
            Self {
                map: std::sync::Mutex::new(std::collections::HashMap::with_hasher(hasher)),
                capacity,
            }
        }

        pub(crate) fn read<R>(&self, key: &K, reader: impl FnOnce(&K, &V) -> R) -> Option<R> {
            let map = self.map.lock().expect("cache lock");
            map.get_key_value(key).map(|(k, v)| reader(k, v))
        }

        /// Wholesale eviction at the bound — cruder than the native LRU, but
        /// bounded, and refill is just cache misses.
        pub(crate) fn put(&self, key: K, value: V) -> Result<(), (K, V)> {
            let mut map = self.map.lock().expect("cache lock");
            if map.len() >= self.capacity {
                map.clear();
            }
            map.insert(key, value);
            Ok(())
        }
    }
}

/// Entries retained before the cache starts evicting.
pub const DEFAULT_CACHE_CAPACITY: usize = 8_192;

/// `HashCache` would round up and allocate too much, so enforced explicitly.
const _: () = assert!(
    DEFAULT_CACHE_CAPACITY.is_power_of_two(),
    "DEFAULT_CACHE_CAPACITY must be a power of two, or HashCache rounds it up"
);

/// A generic read-through cache for a hashmap-backed data structure.
///
/// Anything the value varies with beyond its key belongs in `Meta`, and caches
/// whose metadata disagrees must stay separate maps.
pub struct CacheMap<V, N, Meta>
where
    V: Debug,
    Meta: Debug,
    N: Network,
{
    pub(crate) map: Backing<N::Entry, Arc<V>>,
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
    V: Debug + Send + Sync + 'static,
    N: Network,
    Meta: Default + Debug,
{
    /// Construct this cache with a power-of-two entry bound.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero or not a power of two.
    pub fn with_cache_capacity(capacity: usize) -> Self {
        LockedMap(Arc::new(CacheMap::with_capacity_and_metadata(
            capacity,
            Meta::default(),
        )))
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
        Self::with_capacity_and_metadata(DEFAULT_CACHE_CAPACITY, metadata)
    }

    pub(crate) fn with_capacity_and_metadata(capacity: usize, metadata: Meta) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "cache capacity must be a nonzero power of two"
        );
        Self {
            map: Backing::with_capacity_and_hasher(0, capacity, FxBuildHasher::default()),
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
    use uom::si::f64::Length;
    use uom::si::length::meter;

    /// The weights, given as output from the [`SuccessorsCache::calculate`] function.
    type SuccessorWeights<E> = Vec<(E, DirectionAwareEdgeId<E>, WeightAndDistance)>;

    /// The cache map definition for the successors.
    ///
    /// It accepts a node id as input, from which it will obtain all outgoing
    /// edges accessible to the runtime, and the distance to each one as a
    /// [`WeightAndDistance`].
    pub type SuccessorsCache<N> = LockedMap<SuccessorWeights<<N as DataPlane>::Entry>, N, ()>;

    impl<N: Network> Calculable<N, SuccessorWeights<N::Entry>> for SuccessorsCache<N> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<N>, key: N::Entry) -> SuccessorWeights<N::Entry> {
            // Calc. once
            #[allow(unsafe_code)]
            let source = unsafe { ctx.map.point(&key).unwrap_unchecked() };

            ctx.map
                .edges_outof(key)
                // Only traverse paths accessible to the runtime routing
                // conditions available
                .filter(|(_, _, (_, edge))| match ctx.map.metadata(&edge.index()) {
                    Some(meta) => meta.accessible(ctx.runtime, edge.direction()),
                    None => false,
                })
                .map(|(_, next, (w, edge))| {
                    #[allow(unsafe_code)]
                    let position = unsafe { ctx.map.point(&next).unwrap_unchecked() };

                    // `Haversine` answers in metres; the unit goes on here so
                    // nothing downstream has to remember that.
                    let distance = Length::new::<meter>(Haversine.distance(source, position));
                    (next, distance, w, edge)
                })
                .map(|(next, distance, weight, edge)| {
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
    use core::marker::PhantomData;
    use routers_network::Network;
    use uom::si::f64::Length;

    use super::*;

    /// How far the bounded Dijkstra reaches from its root — one kilometre of
    /// network distance, not straight-line.
    ///
    /// Spelled as a literal because [`Length::new`] is not `const`; a
    /// quantity's field holds its value in the SI base unit, so this is
    /// metres.
    pub const DEFAULT_REACH_DISTANCE: Length = Length {
        dimension: PhantomData,
        units: PhantomData,
        value: 1_000.0,
    };

    #[derive(Debug)]
    pub struct PredicateMetadata<N>
    where
        N: Network,
    {
        /// The successors cache used to back the successors and
        /// prevent repeated calculations.
        successors: SuccessorsCache<N>,

        /// How far the solver reaches.
        reach: Length,

        /// The runtime this cache is bound to, set on first query. Its stored
        /// reachability is only valid for that runtime.
        bound_runtime: OnceLock<N::Runtime>,
    }

    impl<N> Default for PredicateMetadata<N>
    where
        N: Network,
    {
        fn default() -> Self {
            Self {
                successors: SuccessorsCache::default(),
                reach: DEFAULT_REACH_DISTANCE,
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
    /// cache's `reach_distance`, mapped to the node it was reached from.
    /// Computed once on first query and read thereafter — and deterministic,
    /// which is what lets collapse re-derive hop geometry rather than store
    /// it.
    ///
    /// Every entry is computed at the cache's reach, so the reach is fixed at
    /// construction. Build one with `with_reach_distance` to match at anything
    /// other than [`DEFAULT_REACH_DISTANCE`], and pass it to
    /// [`MatchOptions::with_cache`](crate::MatchOptions::with_cache) — which
    /// also keeps it warm across matches.
    pub type PredicateCache<N> =
        LockedMap<Predicates<<N as DataPlane>::Entry>, N, PredicateMetadata<N>>;

    impl<N: Network> PredicateCache<N> {
        /// An empty reachability cache with an explicit entry bound and the
        /// default reach distance. The nested successor cache retains its
        /// independent default bound.
        ///
        /// # Panics
        ///
        /// Panics if `capacity` is zero or not a power of two.
        pub fn with_capacity(capacity: usize) -> Self {
            Self::with_capacities(capacity, DEFAULT_CACHE_CAPACITY)
        }

        /// Set independent entry bounds for reachability results and the
        /// successor lists used while calculating a cache miss.
        ///
        /// # Panics
        ///
        /// Panics if either bound is zero or not a power of two.
        pub fn with_capacities(predicates: usize, successors: usize) -> Self {
            LockedMap(Arc::new(CacheMap::with_capacity_and_metadata(
                predicates,
                PredicateMetadata {
                    successors: SuccessorsCache::with_cache_capacity(successors),
                    reach: DEFAULT_REACH_DISTANCE,
                    bound_runtime: OnceLock::new(),
                },
            )))
        }

        /// An empty cache whose entries reach `reach_distance`.
        ///
        /// Raise it for sparse traces — long gaps between positions, or
        /// motorway driving. The cost is superlinear: a Dijkstra reaching
        /// twice as far settles far more than twice the nodes.
        pub fn with_reach_distance(reach_distance: Length) -> Self {
            LockedMap(Arc::new(CacheMap::with_metadata(PredicateMetadata {
                successors: SuccessorsCache::default(),
                reach: reach_distance,
                bound_runtime: OnceLock::new(),
            })))
        }

        /// How far this cache's entries reach.
        pub fn reach_distance(&self) -> Length {
            self.0.metadata.reach
        }
    }

    impl<N: Network> Calculable<N, Predicates<N::Entry>> for PredicateCache<N> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<N>, key: N::Entry) -> Predicates<N::Entry> {
            // Accessibility, and so every reachability map stored here, is a
            // function of the runtime; a second runtime needs a second cache.
            let bound = self
                .0
                .metadata
                .bound_runtime
                .get_or_init(|| ctx.runtime.clone());

            assert!(
                bound == ctx.runtime,
                "PredicateCache reused across runtimes: its cached reachability was \
                 resolved for a different routing configuration. Use one cache per runtime."
            );

            let reach = self.0.metadata.reach;

            Dijkstra
                .reach(&key, move |node| {
                    ArcIter::new(self.0.metadata.successors.query(ctx, *node))
                        .map(|(a, _, b)| (a, b))
                })
                .take_while(|p| p.total_cost.distance() < reach)
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

pub use predicate::{DEFAULT_REACH_DISTANCE, PredicateCache};
pub use successor::SuccessorsCache;

use crate::primitives::RoutingContext;

#[cfg(test)]
mod tests {
    use super::{Arc, DEFAULT_CACHE_CAPACITY, DEFAULT_REACH_DISTANCE, PredicateCache};
    use routers_network::mock::{MockEntryId, MockNetwork};
    use uom::si::f64::Length;
    use uom::si::length::{centimeter, meter};

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

    #[test]
    fn predicate_cache_accepts_an_explicit_power_of_two_bound() {
        let cache = PredicateCache::<MockNetwork>::with_capacity(16_384);
        for key in 0..32_768 {
            let _ = cache.0.map.put(MockEntryId(key), Arc::default());
        }
        assert_eq!(cache.0.map.capacity(), 16_384);
    }

    #[test]
    #[should_panic(expected = "cache capacity must be a nonzero power of two")]
    fn predicate_cache_rejects_a_rounded_capacity() {
        let _ = PredicateCache::<MockNetwork>::with_capacity(12_000);
    }

    /// A cache reports the reach it was built at, whichever unit named it —
    /// and an unconfigured one reaches the documented default.
    #[test]
    fn a_cache_reports_the_reach_it_was_built_at() {
        let default = PredicateCache::<MockNetwork>::default();
        assert_eq!(default.reach_distance(), DEFAULT_REACH_DISTANCE);
        assert_eq!(default.reach_distance().get::<meter>(), 1_000.0);

        let wide =
            PredicateCache::<MockNetwork>::with_reach_distance(Length::new::<meter>(8_000.0));
        assert_eq!(wide.reach_distance().get::<centimeter>(), 800_000.0);

        // The unit is carried, not assumed: 800,000cm names the same reach.
        let same = PredicateCache::<MockNetwork>::with_reach_distance(Length::new::<centimeter>(
            800_000.0,
        ));
        assert_eq!(same.reach_distance(), wide.reach_distance());
    }
}
