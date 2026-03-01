use alloc::sync::Arc;
use core::fmt::Debug;
use geo::Distance;
use routers_network::{Entry, Metadata, Network};
use rustc_hash::{FxBuildHasher, FxHashMap};
use scc::HashMap;

pub trait CacheKey: Entry {}
impl<T> CacheKey for T where T: Entry {}

/// A generic read-through cache for a hashmap-backed data structure
#[derive(Debug)]
pub struct CacheMap<K, V, M, N, Meta>
where
    K: CacheKey,
    V: Debug,
    M: Metadata,
    Meta: Debug,
    N: Network<K, M>,
{
    pub(crate) map: HashMap<K, Arc<V>, FxBuildHasher>,
    pub(crate) metadata: Meta,

    _marker: core::marker::PhantomData<M>,
    _marker2: core::marker::PhantomData<N>,
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
    V: Debug,
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
        if let Some(value) = self.0.map.get(&key) {
            return Arc::clone(value.get());
        }

        let calculated = Arc::new(self.calculate(ctx, key));
        let _ = self.0.map.insert(key, calculated.clone());

        Arc::clone(&calculated)
    }
}

impl<K, V, M, N, Meta> Default for CacheMap<K, V, M, N, Meta>
where
    K: CacheKey,
    V: Debug,
    M: Metadata,
    N: Network<K, M>,
    Meta: Default + Debug,
{
    fn default() -> Self {
        Self {
            map: HashMap::default(),
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
    use crate::{primitives::WeightAndDistance, transition::*};

    use geo::Haversine;
    use routers_network::DirectionAwareEdgeId;

    /// The weights, given as output from the [`SuccessorsCache::calculate`] function.
    type SuccessorWeights<E> = Vec<(E, DirectionAwareEdgeId<E>, WeightAndDistance)>;

    /// The cache map definition for the successors.
    ///
    /// It accepts a [`NodeIx`] as input, from which it will obtain all outgoing
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
                    let fraction = WeightAndDistance::new(Fraction::mul(weight), distance);

                    (next, edge, fraction)
                })
                .collect::<Vec<_>>()
        }
    }
}

mod predicate {
    use crate::{WeightAndDistance, primitives::Dijkstra};
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
    /// and the pair of corresponding [`NodeIx`] and [`WeightAndDistance`] values
    /// which are reachable from the input index after performing an upper-bounded
    /// dijkstra calculation
    ///
    /// The output from the [`PredicateCache::calculate`] function.
    type Predicates<E> = FxHashMap<E, (E, WeightAndDistance)>;

    /// The predicate cache through which a backing of [`Predicates`] is
    /// made from a [`NodeIx`] key, cached on first calculation and read thereafter.
    pub type PredicateCache<E, M, N> =
        LockedMap<E, Predicates<E>, M, N, PredicateMetadata<E, M, N>>;

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
                    (p.total_cost.1 as f64) < threshold
                })
                .map(|pre| {
                    let parent = pre.parent.unwrap_or_default();
                    (pre.node, (parent, pre.total_cost))
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
