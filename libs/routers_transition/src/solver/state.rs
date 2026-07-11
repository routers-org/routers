//! The caller-owned state of an in-progress match.
//!
//! A [`MatchState`] bundles everything a match accumulates across solves: the
//! [`Trellis`] being grown, the [`SideTable`] of routed hops, and the
//! [`ViterbiSolver`] whose cached forward pass makes re-solving after an
//! append incremental (SPEC §7). Owning all three in one place is what makes
//! the resume sound: the trellis can only grow through
//! [`extend`](MatchState::extend) and only be weighed through a
//! [`Solver`](crate::Solver), so the solver scratch can never be replayed
//! against a foreign trellis.

use crate::{MatchError, Reachable, SideTable};
use routers_network::Entry;
use routers_trellis::{LayerId, Path, Trellis, TrellisError, ViterbiSolver};

/// The accumulated state of one logical match, owned by the caller and handed
/// to a [`Solver`](crate::Solver) each time new data arrives.
///
/// A batch caller lets [`Solver::solve`](crate::Solver::solve) drive it in one
/// shot. A realtime caller keeps one `MatchState` per trip and, per arriving
/// position: [`Transition::push`](crate::Transition::push) →
/// [`extend`](MatchState::extend) →
/// [`Solver::solve_path`](crate::Solver::solve_path). Each such round weighs
/// only the new boundary and resumes the forward pass from it, so the cost of
/// an append is proportional to the new layer — not the whole history.
pub struct MatchState<E>
where
    E: Entry,
{
    /// The trellis grown so far; `None` until the first layer arrives.
    trellis: Option<Trellis>,

    /// Every routed hop weighed so far, keyed by candidate pair — the
    /// side-data joined back onto the solved route at collapse time.
    side: SideTable<E>,

    /// The reused DP solver; its scratch holds the cached forward pass.
    viterbi: ViterbiSolver,

    /// Layers covered by the cached forward pass (0 = nothing cached). This is
    /// the caller-owned resume watermark of SPEC §7.1: it only ever advances on
    /// a successful solve of this state's own trellis.
    solved: usize,
}

impl<E> Default for MatchState<E>
where
    E: Entry,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<E> MatchState<E>
where
    E: Entry,
{
    /// An empty state: no layers, nothing cached.
    pub fn new() -> Self {
        MatchState {
            trellis: None,
            side: SideTable::default(),
            viterbi: ViterbiSolver::new(),
            solved: 0,
        }
    }

    /// Adopt an existing trellis (e.g. pre-built by [`Transition::trellis`]
    /// (crate::Transition::trellis), or deserialized from upstream). The
    /// forward-pass cache starts cold, so the first solve is a full one.
    pub fn from_trellis(trellis: Trellis) -> Self {
        MatchState {
            trellis: Some(trellis),
            ..Self::new()
        }
    }

    /// The trellis grown so far, if any layer has arrived.
    pub fn trellis(&self) -> Option<&Trellis> {
        self.trellis.as_ref()
    }

    /// Number of layers the state currently spans.
    pub fn layers(&self) -> usize {
        self.trellis.as_ref().map_or(0, Trellis::layers)
    }

    /// Append one layer of `width` candidates (its boundary starts pending).
    /// The first call creates the trellis.
    pub fn extend(&mut self, width: u32) -> Result<(), TrellisError> {
        match &mut self.trellis {
            Some(trellis) => trellis.add_layer(width),
            None => {
                self.trellis = Some(Trellis::new(vec![width])?);
                Ok(())
            }
        }
    }

    /// The routed hop between two chosen candidates, if it was weighed.
    pub fn hop(&self, from: crate::CandidateId, to: crate::CandidateId) -> Option<&Reachable<E>> {
        self.side.get(&(from, to))
    }

    // ---- crate-internal seams used by the `Solver` pipeline ----

    /// The trellis and side-table, mutable, for weighing. Errors while no
    /// layer has arrived yet.
    pub(crate) fn weighable(&mut self) -> Result<(&mut Trellis, &mut SideTable<E>), TrellisError> {
        match &mut self.trellis {
            Some(trellis) => Ok((trellis, &mut self.side)),
            None => Err(TrellisError::Empty),
        }
    }

    pub(crate) fn side(&self) -> &SideTable<E> {
        &self.side
    }

    /// Run the DP over the current trellis, resuming from the lowest boundary
    /// `weigh` changed this round (`None` = nothing changed). The watermark
    /// advances only on success, so a failed solve simply resumes earlier next
    /// time.
    pub(crate) fn run(&mut self, changed: Option<LayerId>) -> Result<Path, MatchError> {
        let trellis = self.trellis.as_ref().ok_or(TrellisError::Empty)?;

        // The cached forward pass covers layers below `solved`; a boundary can
        // only resume where its source layer's distance is still valid.
        let cap = self.solved.min(trellis.layers()).saturating_sub(1);
        let resume = changed.map_or(cap, |boundary| boundary.index().min(cap));

        let path = self
            .viterbi
            .solve_resuming(trellis, LayerId(resume as u32))?;

        self.solved = trellis.layers();
        Ok(path)
    }
}
