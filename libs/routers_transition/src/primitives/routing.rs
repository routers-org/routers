use routers_network::{Edge, Network};

use crate::candidate::{Candidate, CandidateRef, CandidateStore};

/// The read-only world a match is computed against: the map, its runtime,
/// and every candidate considered so far.
///
/// Weighers and costing strategies receive one of these rather than bare map
/// references, so an extension point sees exactly what the built-in pipeline
/// sees.
#[derive(Clone, Copy, Debug)]
pub struct RoutingContext<'a, N>
where
    N: Network + ?Sized,
{
    pub candidates: &'a CandidateStore<N::Entry>,
    pub map: &'a N,
    pub runtime: &'a N::Runtime,
}

impl<N> RoutingContext<'_, N>
where
    N: Network + ?Sized,
{
    /// Obtain a [candidate](Candidate), should it exist, by its [ref](CandidateRef).
    pub fn candidate(&self, candidate: &CandidateRef) -> Option<Candidate<N::Entry>> {
        self.candidates.candidate(candidate)
    }

    /// Obtain the [edge](Edge), should it exist, between two nodes (specified as ids).
    pub fn edge(&self, a: &N::Entry, b: &N::Entry) -> Option<Edge<N::Entry>> {
        self.map.edge(a, b)
    }
}
