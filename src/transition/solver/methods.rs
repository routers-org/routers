use crate::transition::*;
use codec::Metadata;
use codec::primitive::Entry;
use itertools::Either;
use rustc_hash::FxHashMap;
use std::hash::Hash;

#[derive(Debug, Default, Copy, Clone)]
pub enum ResolutionMethod {
    #[default]
    Standard,
    DistanceOnly,
}

/// Defines a [target](#field.target) element reachable from some given
/// [source](#field.source) through a known [path](#field.path).
///
/// It requests itself to be resolved in the heuristic-layer by a given
/// [resolution_method](#field.resolution_method).
#[derive(Clone, Debug)]
pub struct Reachable<E>
where
    E: Entry,
{
    pub source: CandidateId,
    pub target: CandidateId,
    pub path: Vec<Edge<E>>, // TODO: => Helper method to remove the duplicate node id's to crt8 a vec<e>

    pub(crate) resolution_method: ResolutionMethod,
}

impl<E> Reachable<E>
where
    E: Entry,
{
    /// Creates a new reachable element, supplied a source, target and path.
    ///
    /// This assumes the default resolution method.
    pub fn new(source: CandidateId, target: CandidateId, path: Vec<Edge<E>>) -> Self {
        Self {
            source,
            target,
            path,
            resolution_method: Default::default(),
        }
    }

    /// Consumes and modifies a reachable element to request the
    /// [`DistanceOnly`](ResolutionMethod::DistanceOnly) option.
    pub fn distance_only(self) -> Self {
        Self {
            resolution_method: ResolutionMethod::DistanceOnly,
            ..self
        }
    }

    /// A collection of all nodes within the reachable's path.
    /// This represents the path as a collection of nodes, as opposed
    /// to the default representation being a collection of edges.
    pub fn path_nodes(&self) -> impl Iterator<Item = E> {
        match self.path.last() {
            Some(last) => Either::Left(
                self.path
                    .iter()
                    .map(|edge| edge.source)
                    .chain(std::iter::once(last.target)),
            ),
            None => Either::Right(std::iter::empty()),
        }
    }

    /// Converts a reachable element into a (source, target) index pair
    /// used for hashing the structure as a path lookup between the
    /// source and target.
    pub fn hash(&self) -> (usize, usize) {
        (self.source.index(), self.target.index())
    }
}

/// Defines a structure which can be supplied to the [`Transition::solve`] function
/// in order to solve the transition graph.
///
/// Functionality is implemented using the [`Solver::solve`] method.
pub trait Solver<E, M>
where
    E: Entry,
    M: Metadata,
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
        transition: Transition<Emmis, Trans, E, M>,
        runtime: &M::Runtime,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E, M> + Send + Sync;

    /// Creates a path from the source up the parent map until no more parents
    /// are found. This assumes there is only one relation between parent and children.
    ///
    /// Returns in the order `[target, ..., source]`.
    ///
    /// If the target is not found by the builder, `None` is returned.
    #[inline]
    fn path_builder<N, C>(source: &N, target: &N, parents: &FxHashMap<N, (N, C)>) -> Option<Vec<N>>
    where
        N: Eq + Hash + Copy,
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
