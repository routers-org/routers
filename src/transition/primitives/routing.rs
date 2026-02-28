use routers_network::{Edge, Entry, Metadata, Network};

use crate::candidate::{Candidate, CandidateId, Candidates};

/// A base context provided to costing methods.
///
/// Allows costing methods to access to further information
/// within the current routing progress at the discretion
/// of the call site.
///
/// Provides access to the base map [`map`](#field.map).
/// It also provides a reference to the [`candidates`](#field.candidates) chosen in prior stages.
#[derive(Clone, Copy, Debug)]
pub struct RoutingContext<'a, E, M>
where
    E: Entry + 'a,
    M: Metadata + 'a,
{
    pub candidates: &'a Candidates<E>,
    pub map: &'a dyn Network<E, M>,
    pub runtime: &'a M::Runtime,
}

impl<E, M> RoutingContext<'_, E, M>
where
    E: Entry,
    M: Metadata,
{
    /// Obtain a [candidate](Candidate), should it exist, by its [identifier](CandidateId).
    pub fn candidate(&self, candidate: &CandidateId) -> Option<Candidate<E>> {
        self.candidates.candidate(candidate)
    }

    /// Obtain the [edge](Edge), should it exist, between two [nodes](NodeIx) (specified as ids)
    pub fn edge(&self, a: &E, b: &E) -> Option<Edge<E>> {
        self.map.edge(a, b)
    }
}
