use crate::Graph;
use crate::transition::Reachable;
use crate::transition::candidate::*;
use geo::LineString;
use routers_codec::{Entry, Metadata};

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
    ) -> Self {
        Self {
            cost,
            interpolated,
            route,
            candidates,
        }
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

    /// Returns the interpolated route from the collapse as a [`LineString`].
    /// This can therefore be used to show the expected turn decisions made by the provided input.
    pub fn interpolated<M: Metadata>(&self, map: &Graph<E, M>) -> LineString {
        self.interpolated
            .iter()
            .enumerate()
            .flat_map(|(index, reachable)| {
                let source = self.candidates.candidate(&reachable.source).unwrap();
                let target = self.candidates.candidate(&reachable.target).unwrap();

                let path = reachable
                    .path_nodes()
                    .filter_map(|node| map.get_position(&node));

                std::iter::repeat_n(source.position, if index == 0 { 1 } else { 0 })
                    .chain(path)
                    .chain(std::iter::once(target.position))
            })
            .collect::<LineString>()
    }

    pub fn edges(self) -> impl Iterator<Item = Edge<E>> {
        self.interpolated
            .into_iter()
            .flat_map(|reachable| reachable.path)
    }
}
