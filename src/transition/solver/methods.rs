use crate::transition::*;
use core::hash::Hash;
use routers_network::{Entry, Metadata, Network};
use rustc_hash::FxHashMap;

/// Defines a structure which can be supplied to the [`Transition::solve`] function
/// in order to solve the transition graph.
///
/// Functionality is implemented using the [`Solver::solve`] method.
pub trait Solver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    /// Refines a single node within an initial layer to all nodes in the
    /// following layer with their respective emission and transition
    /// probabilities in the hidden markov model.
    ///
    /// It may return a match error which is encountered for various reasons.
    /// This may be due to insufficient candidates for a given node in the sequence,
    /// or due to blown-out costings. There are other reasons this may occur given
    /// the functionality is statistical and therefore prone to out-of-bound failures
    /// which are less deterministic than a brute-force model.
    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M, N> + Send + Sync;

    /// Creates a path from the source up the parent map until no more parents
    /// are found. This assumes there is only one relation between parent and children.
    ///
    /// Returns in the order `[target, ..., source]`.
    ///
    /// If the target is not found by the builder, `None` is returned.
    #[inline]
    fn path_builder<K, C>(source: &K, target: &K, parents: &FxHashMap<K, (K, C)>) -> Option<Vec<K>>
    where
        K: Eq + Hash + Copy,
    {
        let mut rev = vec![*source];
        let mut next = source;

        while let Some((parent, _)) = parents.get(next) {
            // Located the target
            if *next == *target {
                rev.reverse();
                return Some(rev);
            }

            rev.push(*parent);
            next = parent;
        }

        None
    }
}
