use crate::generation::LayerGeneration;
use crate::*;
use geo::{Distance, Haversine, Point};
use routers_network::{Entry, Metadata, Network};
use routers_trellis::{LayerId, NodeId};

/// Generates the layers within the transition graph.
///
/// Generates the layers of the transition graph, where each layer
/// represents a point in the linestring, and each node in the layer
/// represents a candidate transition point, within the `distance`
/// search radius of the linestring point, which was found by the
/// projection of the linestring point upon the closest edges within this radius.
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
    pub fn new(map: &'a dyn Network<E, M>, emission: &'a Emmis, search_distance: f64) -> Self {
        StandardGenerator {
            map,
            emission,
            search_distance,
        }
    }
}

impl<Emmis, E, M> LayerGeneration<E> for StandardGenerator<'_, E, M, Emmis>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
{
    fn candidates(&self, origin: &Point, layer: usize) -> Vec<Candidate<E>> {
        self.map
            .nearest_nodes_projected(origin, self.search_distance)
            .enumerate()
            .map(|(node, (position, edge))| {
                let location = CandidateRef::new(LayerId(layer as u32), NodeId(node as u32));
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
