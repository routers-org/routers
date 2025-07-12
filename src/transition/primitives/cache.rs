use crate::transition::RoutingContext;
use geo::Distance;
use routers_codec::{Entry, Metadata};
use rustc_hash::FxHashMap;
use std::fmt::Debug;
use std::sync::{Arc, RwLock};

pub trait CacheKey: Entry {}
impl<T> CacheKey for T where T: Entry {}

/// A generic read-through cache for a hashmap-backed data structure
#[derive(Debug)]
pub struct CacheMap<K, V, M, Meta>
where
    K: CacheKey,
    V: Debug,
    M: Metadata,
    Meta: Debug,
{
    map: FxHashMap<K, Arc<V>>,
    metadata: Meta,

    _marker: std::marker::PhantomData<M>,
}

#[derive(Debug)]
pub struct LockedMap<K, V, M, Meta>(Arc<RwLock<CacheMap<K, V, M, Meta>>>)
where
    LockedMap<K, V, M, Meta>: Calculable<K, M, V>,
    M: Metadata,
    K: CacheKey,
    V: Debug,
    Meta: Debug;

impl<K, V, M, Meta> Default for LockedMap<K, V, M, Meta>
where
    LockedMap<K, V, M, Meta>: Calculable<K, M, V>,
    CacheMap<K, V, M, Meta>: Default,
    M: Metadata,
    K: CacheKey,
    V: Debug,
    Meta: Debug,
{
    fn default() -> Self {
        LockedMap(Arc::new(RwLock::new(CacheMap::default())))
    }
}

impl<K, V, M, Meta> Clone for LockedMap<K, V, M, Meta>
where
    LockedMap<K, V, M, Meta>: Calculable<K, M, V>,
    CacheMap<K, V, M, Meta>: Default,
    M: Metadata,
    K: CacheKey,
    V: Debug,
    Meta: Debug,
{
    fn clone(&self) -> Self {
        LockedMap(Arc::clone(&self.0))
    }
}

impl<K, V, M, Meta> LockedMap<K, V, M, Meta>
where
    LockedMap<K, V, M, Meta>: Calculable<K, M, V>,
    M: Metadata,
    K: CacheKey,
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
    pub fn query(&self, ctx: &RoutingContext<K, M>, key: K) -> Arc<V> {
        if let Some(value) = self.0.read().unwrap().map.get(&key) {
            return Arc::clone(value);
        }

        let calculated = Arc::new(self.calculate(ctx, key));
        self.0.write().unwrap().map.insert(key, calculated.clone());

        Arc::clone(&calculated)
    }
}

impl<K, V, M, Meta> Default for CacheMap<K, V, M, Meta>
where
    K: CacheKey,
    V: Debug,
    M: Metadata,
    Meta: Default + Debug,
{
    fn default() -> Self {
        Self {
            map: FxHashMap::default(),
            metadata: Meta::default(),
            _marker: std::marker::PhantomData,
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
pub trait Calculable<K: CacheKey, M: Metadata, V> {
    /// The concrete implementation of the function which derives the
    /// value, [`V`], from the key, [`K`].
    ///
    /// The function parameters include relevant [`RoutingContext`] which
    /// may be required for the calculation.
    fn calculate(&self, ctx: &RoutingContext<K, M>, key: K) -> V;
}

mod successor {
    use super::*;
    use crate::transition::*;

    use geo::Haversine;
    use petgraph::Direction;

    /// The weights, given as output from the [`SuccessorsCache::calculate`] function.
    type SuccessorWeights<E> = Vec<(E, DirectionAwareEdgeId<E>, WeightAndDistance)>;

    /// The cache map definition for the successors.
    ///
    /// It accepts a [`NodeIx`] as input, from which it will obtain all outgoing
    /// edges and obtain the distances to each one as a [`WeightAndDistance`].
    pub type SuccessorsCache<E, M> = LockedMap<E, SuccessorWeights<E>, M, ()>;

    impl<E: CacheKey, M: Metadata> Calculable<E, M, SuccessorWeights<E>> for SuccessorsCache<E, M> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<E, M>, key: E) -> SuccessorWeights<E> {
            // Calc. once
            let source = unsafe { ctx.map.get_position(&key).unwrap_unchecked() };

            ctx.map
                .graph
                .edges_directed(key, Direction::Outgoing)
                .map(|(_, next, (w, edge))| {
                    const METER_TO_CM: f64 = 100.0;

                    let position = unsafe { ctx.map.get_position(&next).unwrap_unchecked() };

                    // In centimeters (1m = 100cm)
                    let distance = Haversine.distance(source, position);
                    (next, (distance * METER_TO_CM) as u32, *w, *edge)
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
    use crate::transition::*;
    use routers_codec::Entry;

    use super::*;

    const DEFAULT_THRESHOLD: f64 = 200_000f64; // 2km in cm

    #[derive(Debug)]
    pub struct PredicateMetadata<E, M>
    where
        E: Entry,
        M: Metadata,
    {
        /// The successors cache used to back the successors and
        /// prevent repeated calculations.
        successors: SuccessorsCache<E, M>,

        /// The threshold by which the solver is bounded, in centimeters.
        threshold_distance: f64,
    }

    impl<E, M> Default for PredicateMetadata<E, M>
    where
        E: Entry,
        M: Metadata,
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
    pub type PredicateCache<E, M> = LockedMap<E, Predicates<E>, M, PredicateMetadata<E, M>>;

    impl<E: CacheKey, M: Metadata> Calculable<E, M, Predicates<E>> for PredicateCache<E, M> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<E, M>, key: E) -> Predicates<E> {
            let threshold = self.0.read().unwrap().metadata.threshold_distance;

            Dijkstra
                .reach(&key, move |node| {
                    ArcIter::new(self.0.read().unwrap().metadata.successors.query(ctx, *node))
                        .filter(|(_, edge, _)| {
                            // Only traverse paths which can be accessed by
                            // the specific runtime routing conditions available
                            let meta = ctx.map.meta(edge);
                            let direction = edge.direction();

                            // TODO: Does not uphold invariant.
                            //       => Idempotency
                            //       The accessibility check is not considered in the key,
                            //       and so may taint other queries by pre-filtering accessible
                            //       paths, which may be accessible with a different runtime
                            //       configuration.

                            meta.accessible(ctx.runtime, direction)
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
pub struct ArcIter<T> {
    data: Arc<Vec<T>>,
    index: usize,
}

impl<T> ArcIter<T> {
    #[inline(always)]
    pub(crate) fn new(data: Arc<Vec<T>>) -> Self {
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
