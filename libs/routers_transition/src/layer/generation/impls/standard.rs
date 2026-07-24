use crate::candidate::CandidateRef;
use crate::costing::{EmissionContext, EmissionStrategy};
use crate::r#match::DEFAULT_SEARCH_DISTANCE;
use crate::{candidate::Candidate, layer::generation::LayerGeneration};
use geo::{Distance, Haversine, Point};
use routers_network::Network;
use routers_trellis::{LayerId, NodeId};

/// The default candidate generator: a radius search projected onto nearby
/// edges.
///
/// Every edge within [`search_distance`](Self::search_distance) of a
/// trajectory point contributes one candidate — the point's projection onto
/// that edge — priced by the supplied emission strategy.
#[derive(Copy, Clone)]
pub struct StandardGenerator<'a, N, Emmis>
where
    N: Network + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
{
    /// The maximum distance by which the generator will search for nodes,
    /// allowing it to find edges which may be comprised of distant nodes.
    ///
    /// This is a square-radius search, so may pick up nodes outside this
    /// distance as the edge may exist at the square-boundary, beyond the
    /// radial-boundary.
    pub search_distance: f64,

    /// The emission heuristics required to generate the layers.
    ///
    /// This is required as a caching technique since the costs for a candidate
    /// need only be calculated once.
    pub emission: &'a Emmis,

    /// The routing map used to pull candidates from, and provide layout context.
    map: &'a N,
}

impl<'a, N, Emmis> StandardGenerator<'a, N, Emmis>
where
    N: Network + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
{
    /// Creates a [`StandardGenerator`] from a map and emission heuristic.
    pub fn new(map: &'a N, emission: &'a Emmis) -> Self {
        StandardGenerator {
            map,
            emission,
            search_distance: DEFAULT_SEARCH_DISTANCE,
        }
    }

    pub fn with_search_distance(mut self, search_distance: f64) -> Self {
        self.search_distance = search_distance;
        self
    }
}

impl<Emmis, N> LayerGeneration<N::Entry> for StandardGenerator<'_, N, Emmis>
where
    N: Network + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
{
    fn candidates(&self, origin: &Point, layer: LayerId) -> Vec<Candidate<N::Entry>> {
        self.map
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
            .collect()
    }
}
