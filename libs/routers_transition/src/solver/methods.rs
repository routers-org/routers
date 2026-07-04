use crate::*;
use routers_network::{Entry, Metadata, Network};

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
        Trans: TransitionStrategy<E> + Send + Sync;
}
