use alloc::borrow::Cow;

use crate::candidate::*;
use crate::primitives::Reachable;
use geo::LineString;
use routers_network::Entry;
use routers_network::Network;

/// A solved map-match: the chosen candidate per input point, plus the routed
/// path between them.
pub struct CollapsedPath<'a, E>
where
    E: Entry,
{
    /// Total cost of the chosen route — a confidence indicator, not a distance.
    pub cost: u32,

    /// The chosen candidate per layer, in order. Resolve to [`Candidate`]s with
    /// [`matched`](Self::matched).
    pub route: Vec<CandidateRef>,

    /// One [`Reachable`] per hop, each holding the routed path between consecutive
    /// chosen candidates. Render it with [`interpolated`](Self::interpolated).
    pub interpolated: Vec<Reachable<E>>,

    /// The candidate store resolving the [`CandidateRef`]s in [`route`](Self::route).
    pub candidates: Cow<'a, CandidateStore<E>>,
}

impl<E> CollapsedPath<'_, E>
where
    E: Entry,
{
    /// Detach from the borrowed trip by cloning the candidate store (a no-op
    /// when it is already owned).
    pub fn into_owned<'b>(self) -> CollapsedPath<'b, E>
    where
        E: 'b,
    {
        CollapsedPath {
            cost: self.cost,
            route: self.route,
            interpolated: self.interpolated,
            candidates: Cow::Owned(self.candidates.into_owned()),
        }
    }

    /// The chosen [`Candidate`] for each matched input point.
    pub fn matched(&self) -> Vec<Candidate<E>> {
        self.route
            .iter()
            .filter_map(|r| self.candidates.candidate(r))
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
    pub fn interpolated(&self, map: &impl Network<Entry = E>) -> LineString {
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
