use crate::candidate::CandidateRef;
use crate::costing::{EmissionContext, EmissionStrategy};
use crate::r#match::DEFAULT_SEARCH_DISTANCE;
use crate::{candidate::Candidate, layer::generation::LayerGeneration};
use geo::{Distance, Haversine, Point};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{LayerId, NodeId};

/// The default candidate generator: a radius search projected onto nearby
/// edges.
///
/// Every edge within [`search_distance`](Self::search_distance) of a
/// trajectory point contributes one candidate — the point's projection onto
/// that edge — priced by the supplied emission strategy.
#[derive(Copy, Clone)]
pub struct StandardGenerator<'a, E, M, Emmis>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
{
    /// The maximum distance by which the generator will search for nodes,
    /// allowing it to find edges which may be comprised of distant nodes.
    ///
    /// This is a square-radius search, so may pick up nodes outside this
    /// distance as the edge may exist at the square-boundary, beyond the
    /// radial-boundary.
    pub search_distance: f64,

    /// The most candidates a layer may hold; `None` admits every edge in
    /// range. Boundary weighing is O(candidates²), so an unbounded layer in
    /// a dense grid dominates solve cost — the cap keeps the k best
    /// candidates by emission cost, bounding the boundary at k² pairs while
    /// preserving the full search reach (a wider radius no longer costs
    /// quadratically, only the selection).
    pub max_candidates: Option<usize>,

    /// The emission heuristics required to generate the layers.
    ///
    /// This is required as a caching technique since the costs for a candidate
    /// need only be calculated once.
    pub emission: &'a Emmis,

    /// The routing map used to pull candidates from, and provide layout context.
    map: &'a dyn Network<E, M>,
}

impl<'a, E, M, Emmis> StandardGenerator<'a, E, M, Emmis>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
{
    /// Creates a [`StandardGenerator`] from a map and emission heuristic.
    pub fn new(map: &'a dyn Network<E, M>, emission: &'a Emmis) -> Self {
        StandardGenerator {
            map,
            emission,
            search_distance: DEFAULT_SEARCH_DISTANCE,
            max_candidates: None,
        }
    }

    pub fn with_search_distance(mut self, search_distance: f64) -> Self {
        self.search_distance = search_distance;
        self
    }

    pub fn with_max_candidates(mut self, max_candidates: usize) -> Self {
        self.max_candidates = Some(max_candidates);
        self
    }
}

impl<Emmis, E, M> LayerGeneration<E> for StandardGenerator<'_, E, M, Emmis>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
{
    fn candidates(&self, origin: &Point, layer: LayerId) -> Vec<Candidate<E>> {
        let mut candidates: Vec<Candidate<E>> = self
            .map
            .nearest_nodes_projected(origin, self.search_distance)
            .enumerate()
            .map(|(node, (position, edge))| {
                let location = CandidateRef::new(layer, NodeId(node as u32));
                let distance = Haversine.distance(position, *origin);
                let emission = self.emission.cost(EmissionContext::new(
                    &position,
                    origin,
                    distance,
                    edge.weight,
                ));

                Candidate::new(edge.thin(), position, emission, location)
            })
            .collect();

        // Keep the k cheapest by emission. Positional identity is restamped
        // by `Trip::push_layer`, so dropping interior candidates is sound.
        if let Some(k) = self.max_candidates
            && candidates.len() > k
        {
            candidates.select_nth_unstable_by_key(k - 1, |candidate| candidate.emission);
            candidates.truncate(k);
        }

        candidates
    }
}
