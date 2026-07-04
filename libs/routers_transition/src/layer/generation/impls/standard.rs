use crate::definition::{Layer, Layers};
use crate::generation::LayerGeneration;
use crate::*;
use geo::{Distance, Haversine, Point};
use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};
use routers_network::{Entry, Metadata, Network};

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
    fn generate(self, input: &[Point]) -> (Layers, Candidates<E>) {
        let per_layer: Vec<Vec<Candidate<E>>> = input
            .into_par_iter()
            .enumerate()
            .map(|(layer_id, origin)| {
                self.map
                    .nearest_nodes_projected(origin, self.search_distance)
                    .enumerate()
                    .map(|(node_id, (position, edge))| {
                        let location = CandidateLocation { layer_id, node_id };
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
            })
            .collect();

        // Assign stable, per-layer-sequential CandidateIds. `coords` mirrors each
        // layer's node ids so it serves as the (LayerId, NodeId) -> CandidateId table.
        let total: usize = per_layer.iter().map(Vec::len).sum();
        let lookup = scc::HashMap::with_capacity(total);
        let mut coords: Vec<Vec<CandidateId>> = Vec::with_capacity(per_layer.len());
        let mut next_id = 0usize;

        let layers: Vec<Layer> = per_layer
            .into_iter()
            .zip(input.iter())
            .map(|(candidates, &origin)| {
                let mut nodes = Vec::with_capacity(candidates.len());
                for candidate in candidates {
                    let id = CandidateId::new(next_id);
                    next_id += 1;
                    let _ = lookup.insert(id, candidate);
                    nodes.push(id);
                }
                coords.push(nodes.clone());
                Layer { nodes, origin }
            })
            .collect();

        (Layers { layers }, Candidates::new(lookup, coords))
    }
}
