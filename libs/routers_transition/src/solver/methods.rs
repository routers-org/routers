use crate::*;
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{Solve, Trellis, ViterbiSolver};

/// A strategy for collapsing a [`Transition`] into a matched [`CollapsedPath`].
///
/// Every solver is trellis-backed and works against a **caller-owned**
/// [`Trellis`]. The one required operation is [`weigh`](Solver::weigh) — filling
/// the trellis's transition weights. How *much* it fills is the axis between
/// strategies (all-compute vs. selective).
///
/// # Trellis semantics
///
/// `weigh` only touches **pending** transitions; already-resolved boundaries are
/// left alone. So a partially-solved trellis is *resumed*, not redone — callers
/// never have to slice a trellis up to avoid re-weighting solved parts.
///
/// # From scratch
///
/// [`solve`](Solver::solve) is a provided method wiring the whole pipeline
/// (weigh → graph solve → reconstruct) for the common "start from nothing" case.
/// Solvers only implement `weigh`; override `solve` for bespoke orchestration.
pub trait Solver<E, M, N>
where
    E: Entry,
    M: Metadata,
    N: Network<E, M>,
{
    /// Fill the **pending** transitions of `trellis` with weights, recording the
    /// per-edge [`Reachable`] side-data into `side` (keyed by candidate pair) for
    /// later reconstruction. Resolved transitions are skipped.
    fn weigh<Emmis, Trans>(
        &self,
        transition: &Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
        trellis: &mut Trellis,
        side: &mut SideTable<E>,
    ) -> Result<(), MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync;

    /// Full from-scratch pipeline: weigh every pending transition, solve the graph
    /// with `routers_trellis`, and reconstruct the routed [`CollapsedPath`].
    ///
    /// `trellis` is provided (and owned) by the caller so it can be pre-sized,
    /// inspected, or reused; see [`Transition::solve`] for the convenience entry
    /// that allocates one.
    fn solve<Emmis, Trans>(
        &self,
        transition: Transition<Emmis, Trans, E, M, N>,
        runtime: &M::Runtime,
        trellis: &mut Trellis,
    ) -> Result<CollapsedPath<E>, MatchError>
    where
        Emmis: EmissionStrategy + Send + Sync,
        Trans: TransitionStrategy<E> + Send + Sync,
    {
        let mut side = SideTable::default();
        self.weigh(&transition, runtime, trellis, &mut side)?;

        let path = ViterbiSolver::new()
            .solve(trellis)
            .map_err(|_| MatchError::CollapseFailure(CollapseError::NoPathFound))?;
        if !path.reachable {
            return Err(MatchError::CollapseFailure(CollapseError::NoPathFound));
        }

        let route = transition.route_of(&path);
        Ok(CollapsedPath::assemble(
            path.cost,
            route,
            &side,
            transition.candidates,
        ))
    }
}
