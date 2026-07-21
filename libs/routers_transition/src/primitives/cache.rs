use alloc::sync::Arc;
use core::fmt::Debug;
use core::hash::Hash;
use geo::Distance;
use moka::sync::Cache;
use routers_network::{DirectionAwareEdgeId, Entry, Metadata, Network};
use rustc_hash::{FxBuildHasher, FxHashMap};

use crate::primitives::WeightAndDistance;

pub trait CacheKey: Entry {}
impl<T> CacheKey for T where T: Entry {}

/// Byte weight and default budget for a cache value type.
///
/// The byte-bounded [`CacheMap`] uses this both to size itself and to choose
/// eviction victims by real memory footprint rather than entry count — so the
/// fat predicate reachability maps are evicted ahead of the small successor
/// vectors under one shared bound.
pub trait CacheWeight {
    /// Estimated heap bytes this value occupies.
    fn weight_bytes(&self) -> u32;

    /// Environment variable overriding this cache's byte budget, in MiB.
    const BUDGET_ENV: &'static str;

    /// Budget in MiB used when [`BUDGET_ENV`](Self::BUDGET_ENV) is unset.
    const DEFAULT_BUDGET_MB: u64;
}

impl<E: Entry> CacheWeight for FxHashMap<E, E> {
    #[inline]
    fn weight_bytes(&self) -> u32 {
        // hashbrown holds `capacity` (K, V) slots plus ~1 control byte each,
        // atop a small fixed table header.
        const HEADER: usize = 48;
        let per_slot = core::mem::size_of::<(E, E)>() + 1;
        self.capacity()
            .saturating_mul(per_slot)
            .saturating_add(HEADER)
            .min(u32::MAX as usize) as u32
    }

    const BUDGET_ENV: &'static str = "MATCHER_PREDICATE_CACHE_MB";
    const DEFAULT_BUDGET_MB: u64 = 512;
}

impl<E: Entry> CacheWeight for Vec<(E, DirectionAwareEdgeId<E>, WeightAndDistance)> {
    #[inline]
    fn weight_bytes(&self) -> u32 {
        // Vec header (ptr/len/cap) atop `capacity` inline elements.
        const HEADER: usize = 24;
        let per_elem = core::mem::size_of::<(E, DirectionAwareEdgeId<E>, WeightAndDistance)>();
        self.capacity()
            .saturating_mul(per_elem)
            .saturating_add(HEADER)
            .min(u32::MAX as usize) as u32
    }

    const BUDGET_ENV: &'static str = "MATCHER_SUCCESSOR_CACHE_MB";
    const DEFAULT_BUDGET_MB: u64 = 128;
}

/// Builds a byte-bounded, recency-aware cache: `moka` evicts by
/// [`CacheWeight::weight_bytes`] once the total weight exceeds `max_bytes`, so
/// the footprint is capped in memory rather than in entry count. Values live
/// behind an `Arc`, so an eviction only drops the cache's own reference — any
/// in-flight query keeps its result alive until it is done with it.
fn build_cache<K, V>(max_bytes: u64) -> Cache<K, Arc<V>, FxBuildHasher>
where
    K: Hash + Eq + Send + Sync + Clone + 'static,
    V: CacheWeight + Send + Sync + 'static,
{
    Cache::builder()
        .max_capacity(max_bytes)
        .weigher(|_key, value: &Arc<V>| value.weight_bytes())
        .build_with_hasher(FxBuildHasher)
}

/// Resolves a cache's byte budget from its [`CacheWeight::BUDGET_ENV`] override
/// (interpreted as MiB), falling back to [`CacheWeight::DEFAULT_BUDGET_MB`].
fn budget_bytes<V: CacheWeight>() -> u64 {
    std::env::var(V::BUDGET_ENV)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(V::DEFAULT_BUDGET_MB)
        .saturating_mul(1024 * 1024)
}

/// A generic read-through cache for a hashmap-backed data structure
pub struct CacheMap<K, V, M, N, Meta>
where
    K: CacheKey,
    V: Debug,
    M: Metadata,
    Meta: Debug,
    N: Network<K, M>,
{
    pub(crate) map: Cache<K, Arc<V>, FxBuildHasher>,
    pub(crate) metadata: Meta,

    _marker: core::marker::PhantomData<M>,
    _marker2: core::marker::PhantomData<N>,
}

// Hand-rolled so the `Debug` bound stays off the moka cache (whose own `Debug`,
// and its stats accessors, would drag `V: Send + Sync + 'static` onto every use
// of `CacheMap`). The entries themselves are elided.
impl<K, V, M, N, Meta> Debug for CacheMap<K, V, M, N, Meta>
where
    K: CacheKey,
    V: Debug,
    M: Metadata,
    Meta: Debug,
    N: Network<K, M>,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("CacheMap")
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct LockedMap<K, V, M, N, Meta>(Arc<CacheMap<K, V, M, N, Meta>>)
where
    LockedMap<K, V, M, N, Meta>: Calculable<K, M, N, V>,
    M: Metadata,
    K: CacheKey,
    N: Network<K, M>,
    V: Debug,
    Meta: Debug;

impl<K, V, M, N, Meta> Default for LockedMap<K, V, M, N, Meta>
where
    LockedMap<K, V, M, N, Meta>: Calculable<K, M, N, V>,
    CacheMap<K, V, M, N, Meta>: Default,
    M: Metadata,
    K: CacheKey,
    V: Debug,
    N: Network<K, M>,
    Meta: Debug,
{
    fn default() -> Self {
        LockedMap(Arc::new(CacheMap::default()))
    }
}

impl<K, V, M, N, Meta> Clone for LockedMap<K, V, M, N, Meta>
where
    LockedMap<K, V, M, N, Meta>: Calculable<K, M, N, V>,
    CacheMap<K, V, M, N, Meta>: Default,
    N: Network<K, M>,
    M: Metadata,
    K: CacheKey,
    V: Debug,
    Meta: Debug,
{
    fn clone(&self) -> Self {
        LockedMap(Arc::clone(&self.0))
    }
}

impl<K, V, N, M, Meta> LockedMap<K, V, M, N, Meta>
where
    LockedMap<K, V, M, N, Meta>: Calculable<K, M, N, V>,
    M: Metadata,
    K: CacheKey,
    N: Network<K, M>,
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
    /// consumes an owned value of the key, [`K`], which is required for the
    /// call to the [`Calculable::calculate`] function.
    pub fn query(&self, ctx: &RoutingContext<K, M, N>, key: K) -> Arc<V> {
        // Read-through: `get_with` returns the cached value or runs the
        // closure once (deduping concurrent misses on the same key), and
        // eviction is handled by the byte bound configured at build time.
        self.0
            .map
            .get_with(key, || Arc::new(self.calculate(ctx, key)))
    }
}

impl<K, V, M, N, Meta> Default for CacheMap<K, V, M, N, Meta>
where
    K: CacheKey,
    V: CacheWeight + Debug + Send + Sync + 'static,
    M: Metadata,
    N: Network<K, M>,
    Meta: Default + Debug,
{
    fn default() -> Self {
        Self {
            map: build_cache::<K, V>(budget_bytes::<V>()),
            metadata: Meta::default(),

            _marker: core::marker::PhantomData,
            _marker2: core::marker::PhantomData,
        }
    }
}

/// Implementation of a routing-domain calculable KV pair.
///
/// Asserts that the value, [`V`] can be generated from the key, [`K`],
/// given routing context, and the base structure.
///
/// ### Examples
///
/// The [`SuccessorsCache`] and [`PredicateCache`] are both examples
/// of calculable elements.
///
/// The [`SuccessorsCache`], given an underlying map key,
/// can derive the successors using the routing map and an
/// upper-bounded dijkstra algorithm.
pub trait Calculable<E: CacheKey, M: Metadata, N: Network<E, M>, V> {
    /// The concrete implementation of the function which derives the
    /// value, [`V`], from the key, [`K`].
    ///
    /// The function parameters include relevant [`RoutingContext`] which
    /// may be required for the calculation.
    fn calculate(&self, ctx: &RoutingContext<E, M, N>, key: E) -> V;
}

mod successor {
    use super::*;
    use crate::primitives::WeightAndDistance;

    use geo::Haversine;
    use routers_network::DirectionAwareEdgeId;

    /// The weights, given as output from the [`SuccessorsCache::calculate`] function.
    type SuccessorWeights<E> = Vec<(E, DirectionAwareEdgeId<E>, WeightAndDistance)>;

    /// The cache map definition for the successors.
    ///
    /// It accepts a node id as input, from which it will obtain all outgoing
    /// edges and obtain the distances to each one as a [`WeightAndDistance`].
    pub type SuccessorsCache<E, M, N> = LockedMap<E, SuccessorWeights<E>, M, N, ()>;

    impl<E: CacheKey, M: Metadata, N: Network<E, M>> Calculable<E, M, N, SuccessorWeights<E>>
        for SuccessorsCache<E, M, N>
    {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<E, M, N>, key: E) -> SuccessorWeights<E> {
            // Calc. once
            #[allow(unsafe_code)]
            let source = unsafe { ctx.map.point(&key).unwrap_unchecked() };

            ctx.map
                .edges_outof(key)
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
    use crate::primitives::{Dijkstra, algorithms::DijkstraReachableItem};
    use routers_network::{Entry, Network};

    use super::*;

    const DEFAULT_THRESHOLD: f64 = 200_000f64; // 2km in cm

    #[derive(Debug)]
    pub struct PredicateMetadata<E, M, N>
    where
        E: Entry,
        M: Metadata,
        N: Network<E, M>,
    {
        /// The successors cache used to back the successors and
        /// prevent repeated calculations.
        successors: SuccessorsCache<E, M, N>,

        /// The threshold by which the solver is bounded, in centimeters.
        threshold_distance: f64,
    }

    impl<E, M, N> Default for PredicateMetadata<E, M, N>
    where
        E: Entry,
        M: Metadata,
        N: Network<E, M>,
    {
        fn default() -> Self {
            Self {
                successors: SuccessorsCache::default(),
                threshold_distance: DEFAULT_THRESHOLD,
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
    pub type PredicateCache<E, M, N> =
        LockedMap<E, Predicates<E>, M, N, PredicateMetadata<E, M, N>>;

    impl<E: CacheKey, M: Metadata, N: Network<E, M>> PredicateCache<E, M, N> {
        pub fn with_threshold(threshold_cm: f64) -> Self {
            LockedMap(Arc::new(CacheMap {
                map: build_cache::<E, Predicates<E>>(budget_bytes::<Predicates<E>>()),
                metadata: PredicateMetadata {
                    successors: SuccessorsCache::default(),
                    threshold_distance: threshold_cm,
                },
                _marker: core::marker::PhantomData,
                _marker2: core::marker::PhantomData,
            }))
        }
    }

    impl<E: CacheKey, M: Metadata, N: Network<E, M>> Calculable<E, M, N, Predicates<E>>
        for PredicateCache<E, M, N>
    {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<E, M, N>, key: E) -> Predicates<E> {
            let threshold = self.0.metadata.threshold_distance;

            Dijkstra
                .reach(&key, move |node| {
                    ArcIter::new(self.0.metadata.successors.query(ctx, *node))
                        .filter(|(_, edge, _)| {
                            // Only traverse paths which can be accessed by
                            // the specific runtime routing conditions available
                            let meta = ctx.map.metadata(&edge.index());
                            if meta.is_none() {
                                return false;
                            }

                            let direction = edge.direction();

                            // TODO: Does not uphold invariant.
                            //       => Idempotency
                            //       The accessibility check is not considered in the key,
                            //       and so may taint other queries by pre-filtering accessible
                            //       paths, which may be accessible with a different runtime
                            //       configuration.

                            meta.unwrap().accessible(ctx.runtime, direction)
                        })
                        .map(|(a, _, b)| (a, b))
                })
                .take_while(|p| {
                    // Bounded by the threshold distance (centimeters)
                    (p.total_cost.distance_cm() as f64) < threshold
                })
                .map(|DijkstraReachableItem { node, parent, .. }| {
                    (node, parent.unwrap_or_default())
                })
                .collect::<Predicates<E>>()
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
