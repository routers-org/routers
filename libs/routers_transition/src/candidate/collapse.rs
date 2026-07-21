use alloc::borrow::Cow;

use crate::candidate::*;
use crate::primitives::Reachable;
use geo::LineString;
use routers_network::Network;
use routers_network::{Entry, Metadata};

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
    pub fn interpolated<M: Metadata>(&self, map: &impl Network<E, M>) -> LineString {
        self.interpolated_segments(map)
            .into_iter()
            .flat_map(|segment| segment.0)
            .collect()
    }

    /// The driven path split per matched observation: `segments[i]` is the road
    /// geometry arriving at observation `i` (the roads driven since observation
    /// `i - 1`), ending at its matched position. Observation 0 carries only its
    /// matched position, having no incoming hop.
    ///
    /// Concatenating the segments in order reproduces [`interpolated`](Self::interpolated);
    /// crucially, *any prefix* of them is a self-contained partial trace. A
    /// streaming consumer that has committed observations up to a coalescence
    /// boundary can therefore hold those segments as final and only revise the
    /// tail — the geometry for a committed observation never moves, because it
    /// is the arrival at that observation, not the departure toward the next.
    pub fn interpolated_segments<M: Metadata>(
        &self,
        map: &impl Network<E, M>,
    ) -> Vec<LineString> {
        let mut segments = Vec::with_capacity(self.interpolated.len() + 1);

        // Observation 0: its matched position alone (no hop arrives at it).
        if let Some(first) = self.interpolated.first() {
            let source = self.candidates.candidate(&first.source).unwrap();
            segments.push(LineString::from(vec![source.position.0]));
        }

        // Each hop's routed geometry, ending at the observation it arrives at.
        for reachable in &self.interpolated {
            let target = self.candidates.candidate(&reachable.target).unwrap();
            let path = reachable.path_nodes().filter_map(|node| map.point(&node));
            segments.push(path.chain(core::iter::once(target.position)).collect());
        }

        segments
    }
}
