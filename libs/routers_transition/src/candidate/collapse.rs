use crate::candidate::*;
use crate::{Reachable, SideTable};
use geo::LineString;
use routers_network::Network;
use routers_network::{Entry, Metadata};

/// A solved map-match: the chosen candidate per input point, plus the routed
/// path between them.
pub struct CollapsedPath<E>
where
    E: Entry,
{
    /// Total cost of the chosen route — a confidence indicator, not a distance.
    pub cost: u32,

    /// The chosen candidate per layer, in order. Resolve to [`Candidate`]s with
    /// [`matched`](Self::matched).
    pub route: Vec<CandidateId>,

    /// One [`Reachable`] per hop, each holding the routed path between consecutive
    /// chosen candidates. Render it with [`interpolated`](Self::interpolated).
    pub interpolated: Vec<Reachable<E>>,

    /// Flyweight to resolve the [`CandidateId`]s in [`route`](Self::route).
    pub candidates: Candidates<E>,
}

impl<E> CollapsedPath<E>
where
    E: Entry,
{
    /// Build a collapsed path from a solved candidate `route` and the per-hop
    /// [`SideTable`] gathered while weighing.
    ///
    /// Interleaves one [`Reachable`] per hop, looked up by the `(from, to)` pair;
    /// `route` must contain only real candidates, in layer order.
    pub fn assemble(
        cost: u32,
        route: Vec<CandidateId>,
        side: &SideTable<E>,
        candidates: Candidates<E>,
    ) -> Self {
        let interpolated = route
            .windows(2)
            .filter_map(|hop| match hop {
                [from, to] => side.get(&(*from, *to)).cloned(),
                _ => None,
            })
            .collect::<Vec<_>>();

        Self {
            cost,
            route,
            interpolated,
            candidates,
        }
    }

    /// The chosen [`Candidate`] for each matched input point.
    pub fn matched(&self) -> Vec<Candidate<E>> {
        self.route
            .iter()
            .filter_map(|node| self.candidates.lookup.get(node))
            .map(|can| *can)
            .collect::<Vec<_>>()
    }

    /// The matched candidate positions as a [`LineString`] (one point per input).
    pub fn collapsed(&self) -> LineString {
        self.matched()
            .iter()
            .map(|candidate| candidate.position)
            .collect::<LineString>()
    }

    /// The full driven path as a [`LineString`] — the matched positions with the
    /// routed road geometry between them filled in, showing the turns taken.
    pub fn interpolated<M: Metadata>(&self, map: &impl Network<E, M>) -> LineString {
        self.interpolated
            .iter()
            .enumerate()
            .flat_map(|(index, reachable)| {
                let source = self.candidates.candidate(&reachable.source).unwrap();
                let target = self.candidates.candidate(&reachable.target).unwrap();

                let path = reachable.path_nodes().filter_map(|node| map.point(&node));

                core::iter::repeat_n(source.position, if index == 0 { 1 } else { 0 })
                    .chain(path)
                    .chain(core::iter::once(target.position))
            })
            .collect::<LineString>()
    }
}
