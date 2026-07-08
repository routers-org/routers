use crate::candidate::*;
use crate::{Reachable, SideTable};
use geo::LineString;
use routers_network::Edge;
use routers_network::Network;
use routers_network::{Entry, Metadata};

/// The collapsed solution to a transition graph.
pub struct CollapsedPath<E>
where
    E: Entry,
{
    /// The solved cost of the collapsed route.
    /// This value is not actionable by the consumer but rather indicative of how confident
    /// the system is in the route chosen.
    pub cost: u32,

    /// The route as a vector of [`CandidateId`]s.
    /// To obtain the list of [`Candidate`]s, use [`CollapsedPath::matched`]
    pub route: Vec<CandidateId>,

    /// The interpolated nodes of the collapsed route.
    /// This exists as a vector of [`Reachable`] nodes which represent each layer transition.
    /// Each node contains the interpolated path between the candidates in those layers.
    ///
    /// To obtain the geographic representation of this interpolation,
    /// use the [`CollapsedPath::interpolated`] method.
    pub interpolated: Vec<Reachable<E>>,

    /// All considered routes between candidates, regardless of whether they were
    /// chosen for the final path. This is useful for visualisation and debugging.
    #[cfg(debug_assertions)]
    pub considered: Vec<Reachable<E>>,

    pub candidates: Candidates<E>,
}

impl<E> CollapsedPath<E>
where
    E: Entry,
{
    pub(crate) fn new(
        cost: u32,
        interpolated: Vec<Reachable<E>>,
        route: Vec<CandidateId>,
        candidates: Candidates<E>,
        #[cfg(debug_assertions)] considered: Vec<Reachable<E>>,
    ) -> Self {
        Self {
            cost,
            interpolated,
            route,
            candidates,
            #[cfg(debug_assertions)]
            considered,
        }
    }

    /// Assemble a collapsed path from a solved candidate `route` and the per-edge
    /// [`SideTable`] gathered while weighing.
    ///
    /// Shared by every solver: it interleaves one [`Reachable`] per real
    /// candidate hop (looked up by the `(from, to)` pair) — virtual source/sink
    /// hops simply miss the table and are skipped. `route` must contain only real
    /// candidates, in layer order.
    pub fn assemble(
        cost: u32,
        route: Vec<CandidateId>,
        side: &SideTable<E>,
        candidates: Candidates<E>,
    ) -> Self {
        let interpolated = route
            .windows(2)
            .filter_map(|pair| match pair {
                [a, b] => side.get(&(*a, *b)).cloned(),
                _ => None,
            })
            .collect::<Vec<_>>();

        CollapsedPath::new(
            cost,
            interpolated,
            route,
            candidates,
            #[cfg(debug_assertions)]
            side.values().cloned().collect(),
        )
    }

    /// Returns the vector of [`Candidate`]s involved in a match.
    /// Each candidate represents the matched position of every input node.
    ///
    /// This includes further information such as the edge it matched to,
    /// costing and the identifier for the candidate.
    pub fn matched(&self) -> Vec<Candidate<E>> {
        self.route
            .iter()
            .filter_map(|node| self.candidates.lookup.get(node))
            .map(|can| *can)
            .collect::<Vec<_>>()
    }

    pub fn collapsed(&self) -> LineString {
        self.matched()
            .iter()
            .map(|candidate| candidate.position)
            .collect::<LineString>()
    }

    /// Returns the interpolated route from the collapse as a [`LineString`].
    /// This can therefore be used to show the expected turn decisions made by the provided input.
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

    pub fn edges(self) -> impl Iterator<Item = Edge<E>> {
        self.interpolated
            .into_iter()
            .flat_map(|reachable| reachable.path)
    }
}
