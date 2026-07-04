use crate::{Candidate, CandidateId};
use core::fmt::Debug;
use routers_network::Entry;
use scc::HashMap;

/// Store of the candidates considered for a match.
///
/// Holds a flyweight [`lookup`](#field.lookup) from [`CandidateId`] to the full
/// [`Candidate`], and a per-layer [`coords`](#field.coords) table. The layered
/// structure lives here (rather than in an explicit graph) because every layer is
/// fully connected to the next — so "the successors of a candidate" is simply
/// "every candidate in the following layer".
pub struct Candidates<E>
where
    E: Entry,
{
    /// Candidate flyweight, keyed by [`CandidateId`].
    pub lookup: HashMap<CandidateId, Candidate<E>>,

    /// Per-layer candidate ids. `coords[layer]` lists that layer's candidates in
    /// stable insertion order, so it doubles as the `(LayerId, NodeId) -> CandidateId`
    /// table used to map solver output back to candidates.
    pub coords: Vec<Vec<CandidateId>>,
}

impl<E> Debug for Candidates<E>
where
    E: Entry,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(
            f,
            "Candidates {{ {} entries, {} layers }}",
            self.lookup.len(),
            self.coords.len()
        )
    }
}

impl<E> Candidates<E>
where
    E: Entry,
{
    pub fn new(lookup: HashMap<CandidateId, Candidate<E>>, coords: Vec<Vec<CandidateId>>) -> Self {
        Self { lookup, coords }
    }

    /// Returns all candidates in the layer following the one containing
    /// `candidate`. Layers are fully connected, so every candidate in the next
    /// layer is a successor.
    ///
    /// ```text
    ///             Layer    Layer
    ///               N       N+1
    ///
    ///                __/---+
    ///               /
    ///    SOURCE    +-------+
    ///               \
    ///                ‾‾\---+
    /// ```
    pub fn next_layer(&self, candidate: &CandidateId) -> Vec<CandidateId> {
        let Some(c) = self.candidate(candidate) else {
            return Vec::new();
        };
        self.coords
            .get(c.location.layer_id + 1)
            .cloned()
            .unwrap_or_default()
    }

    /// Obtain a [`Candidate`], should it exist, by its [`CandidateId`].
    pub fn candidate(&self, a: &CandidateId) -> Option<Candidate<E>> {
        self.lookup.get(a).map(|c| *c)
    }
}

impl<E> Default for Candidates<E>
where
    E: Entry,
{
    fn default() -> Self {
        Candidates {
            lookup: HashMap::default(),
            coords: Vec::new(),
        }
    }
}
