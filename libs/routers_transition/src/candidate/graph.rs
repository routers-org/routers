use crate::{Candidate, CandidateId};
use core::fmt::Debug;
use routers_network::Entry;
use scc::HashMap;

/// The flyweight of candidates considered for a match.
///
/// Maps each [`CandidateId`] to its full [`Candidate`]. The layered ordering of
/// candidates lives on the [`Layers`](crate::Layers) the match was generated
/// from; this store only answers "what is candidate `id`?".
pub struct Candidates<E>
where
    E: Entry,
{
    /// Candidate flyweight, keyed by [`CandidateId`].
    pub lookup: HashMap<CandidateId, Candidate<E>>,
}

impl<E> Debug for Candidates<E>
where
    E: Entry,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> Result<(), core::fmt::Error> {
        write!(f, "Candidates {{ {} entries }}", self.lookup.len())
    }
}

impl<E> Candidates<E>
where
    E: Entry,
{
    pub fn new(lookup: HashMap<CandidateId, Candidate<E>>) -> Self {
        Self { lookup }
    }

    /// The [`Candidate`] behind an id, if present.
    pub fn candidate(&self, id: &CandidateId) -> Option<Candidate<E>> {
        self.lookup.get(id).map(|c| *c)
    }
}

impl<E> Default for Candidates<E>
where
    E: Entry,
{
    fn default() -> Self {
        Candidates {
            lookup: HashMap::default(),
        }
    }
}
