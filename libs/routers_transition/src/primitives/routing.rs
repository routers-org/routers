use routers_network::{Edge, Entry, Metadata, Network};

use crate::candidate::{Candidate, CandidateRef, CandidateStore};

/// The read-only world a match is computed against: the map, its runtime,
/// and every candidate considered so far.
///
/// Weighers and costing strategies receive one of these rather than bare map
/// references, so an extension point sees exactly what the built-in pipeline
/// sees.
#[derive(Clone, Copy, Debug)]
pub struct RoutingContext<'a, E, M, N>
where
    E: Entry + 'a,
    M: Metadata + 'a,
    N: Network<E, M>,
{
    pub candidates: &'a CandidateStore<E>,
    pub map: &'a N,
    pub runtime: &'a M::Runtime,
}

impl<N, E, M> RoutingContext<'_, E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    /// Obtain a [candidate](Candidate), should it exist, by its [ref](CandidateRef).
    pub fn candidate(&self, candidate: &CandidateRef) -> Option<Candidate<E>> {
        self.candidates.candidate(candidate)
    }

    /// Obtain the [edge](Edge), should it exist, between two nodes (specified as ids).
    pub fn edge(&self, a: &E, b: &E) -> Option<Edge<E>> {
        self.map.edge(a, b)
    }
}
