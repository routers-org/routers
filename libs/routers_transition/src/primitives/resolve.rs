use itertools::Either;
use routers_network::{Edge, Entry};
use serde::{Deserialize, Serialize};

use crate::candidate::CandidateRef;

#[derive(Debug, Default, Copy, Clone, Serialize, Deserialize)]
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
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct Reachable<E>
where
    E: Entry,
{
    pub source: CandidateRef,
    pub target: CandidateRef,
    pub path: Vec<Edge<E>>,

    pub(crate) resolution_method: ResolutionMethod,
}

impl<E> Reachable<E>
where
    E: Entry,
{
    /// A reachable with an explicit routed `path` and the default (routed)
    /// resolution method.
    pub fn new(source: CandidateRef, target: CandidateRef, path: Vec<Edge<E>>) -> Self {
        Self {
            source,
            target,
            path,
            resolution_method: Default::default(),
        }
    }

    /// A same-edge reachable: no routed path, resolved by
    /// [`DistanceOnly`](ResolutionMethod::DistanceOnly).
    pub fn direct(source: CandidateRef, target: CandidateRef) -> Self {
        Self::new(source, target, Vec::new()).distance_only()
    }

    pub fn candidates(&self) -> (CandidateRef, CandidateRef) {
        (self.source, self.target)
    }

    /// Consumes and modifies a reachable element to request the
    /// [`DistanceOnly`](ResolutionMethod::DistanceOnly) option.
    pub fn distance_only(self) -> Self {
        Self {
            resolution_method: ResolutionMethod::DistanceOnly,
            ..self
        }
    }

    /// The path as its sequence of nodes rather than edges.
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
}
