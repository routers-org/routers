use itertools::Either;
use routers_network::{Edge, Entry};
use rustc_hash::FxHashMap;

use crate::CandidateId;

/// Per-edge side-data gathered while weighing a transition, keyed by the
/// `(source, target)` candidate pair. Solvers stash the [`Reachable`] for each
/// costed transition here so the routed path can be reconstructed from a solved
/// candidate sequence (see [`CollapsedPath::assemble`](crate::CollapsedPath::assemble)).
pub type SideTable<E> = FxHashMap<(CandidateId, CandidateId), Reachable<E>>;

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

    #[cfg(debug_assertions)]
    pub cost: u32,
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
            #[cfg(debug_assertions)]
            cost: 0,
        }
    }

    pub fn candidates<'a>(&'a self) -> (&'a CandidateId, &'a CandidateId) {
        (&self.source, &self.target)
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
                    .chain(core::iter::once(last.target)),
            ),
            None => Either::Right(core::iter::empty()),
        }
    }

    /// Converts a reachable element into a (source, target) index pair
    /// used for hashing the structure as a path lookup between the
    /// source and target.
    pub fn hash(&self) -> (usize, usize) {
        (self.source.index(), self.target.index())
    }
}
