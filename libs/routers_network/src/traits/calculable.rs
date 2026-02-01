use super::entry::Entry;
use super::metadata::Metadata;

pub trait CacheKey: Entry {}
impl<T> CacheKey for T where T: Entry {}

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
pub trait Calculable<K: CacheKey, M: Metadata> {
    type Ctx;

    /// The concrete implementation of the function which derives the
    /// value, [`V`], from the key, [`K`].
    ///
    /// The function parameters include relevant [`RoutingContext`] which
    /// may be required for the calculation.
    fn calculate<V>(&self, ctx: &Self::Ctx, key: K) -> V;
}
