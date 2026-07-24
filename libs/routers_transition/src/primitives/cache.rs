use alloc::sync::Arc;
use core::fmt::Debug;
use geo::Distance;
use routers_network::{DataPlane, Metadata, Network};
use rustc_hash::{FxBuildHasher, FxHashMap};
use scc::HashMap;

/// A generic read-through cache for a hashmap-backed data structure
pub struct CacheMap<V, N, Meta>
where
    V: Debug,
    Meta: Debug,
    N: Network,
{
    pub(crate) map: HashMap<N::Entry, Arc<V>, FxBuildHasher>,
    pub(crate) metadata: Meta,
}

// Hand-rolled so the `Debug` bound stays off the moka cache (whose own `Debug`,
// and its stats accessors, would drag `V: Send + Sync + 'static` onto every use
// of `CacheMap`). The entries themselves are elided.
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
        if let Some(value) = self.0.map.get(&key) {
            return Arc::clone(value.get());
        }

        let calculated = Arc::new(self.calculate(ctx, key));
        let _ = self.0.map.insert(key, calculated.clone());

        Arc::clone(&calculated)
    }
}

impl<V, N, Meta> Default for CacheMap<V, N, Meta>
where
    V: Debug + Send + Sync + 'static,
    N: Network,
    Meta: Default + Debug,
{
    fn default() -> Self {
        Self {
            map: HashMap::default(),
            metadata: Meta::default(),
        }
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

    /// The cache map definition for the successors.
    ///
    /// It accepts a node id as input, from which it will obtain all outgoing
    /// edges and obtain the distances to each one as a [`WeightAndDistance`].
    pub type SuccessorsCache<N> = LockedMap<SuccessorWeights<<N as DataPlane>::Entry>, N, ()>;

    impl<N: Network> Calculable<N, SuccessorWeights<N::Entry>> for SuccessorsCache<N> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<N>, key: N::Entry) -> SuccessorWeights<N::Entry> {
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
    use routers_network::Network;

    use super::*;

    const DEFAULT_THRESHOLD: f64 = 200_000f64; // 2km in cm

    #[derive(Debug)]
    pub struct PredicateMetadata<N>
    where
        N: Network,
    {
        /// The successors cache used to back the successors and
        /// prevent repeated calculations.
        successors: SuccessorsCache<N>,

        /// The threshold by which the solver is bounded, in centimeters.
        threshold_distance: f64,
    }

    impl<N> Default for PredicateMetadata<N>
    where
        N: Network,
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
    pub type PredicateCache<N> =
        LockedMap<Predicates<<N as DataPlane>::Entry>, N, PredicateMetadata<N>>;

    impl<N: Network> PredicateCache<N> {
        pub fn with_threshold(threshold_cm: f64) -> Self {
            LockedMap(Arc::new(CacheMap {
                map: HashMap::default(),
                metadata: PredicateMetadata {
                    successors: SuccessorsCache::default(),
                    threshold_distance: threshold_cm,
                },
            }))
        }
    }

    impl<N: Network> Calculable<N, Predicates<N::Entry>> for PredicateCache<N> {
        #[inline]
        fn calculate(&self, ctx: &RoutingContext<N>, key: N::Entry) -> Predicates<N::Entry> {
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
