use crate::transition::*;
use crate::{Graph, Scan};
use std::collections::HashMap;

use geo::{Distance, Haversine, Point};
use itertools::Itertools;
use petgraph::{Directed, Graph as PetGraph};
use rayon::iter::{IndexedParallelIterator, ParallelBridge, ParallelIterator};
use rayon::prelude::{FromParallelIterator, IntoParallelIterator};
use routers_codec::{Entry, Metadata};

#[derive(Default)]
pub struct Layers {
    pub layers: Vec<Layer>,
}

impl Layers {
    pub fn last(&self) -> Option<&Layer> {
        self.layers.last()
    }

    pub fn first(&self) -> Option<&Layer> {
        self.layers.first()
    }
}

impl FromParallelIterator<Layer> for Layers {
    fn from_par_iter<I>(layers: I) -> Self
    where
        I: IntoParallelIterator<Item = Layer>,
    {
        let layers = layers.into_par_iter().collect::<Vec<Layer>>();
        Self { layers }
    }
}

const DEFAULT_SEARCH_DISTANCE: f64 = 50.; // 50m

/// Generates the layers within the transition graph.
///
/// Generates the layers of the transition graph, where each layer
/// represents a point in the linestring, and each node in the layer
/// represents a candidate transition point, within the `distance`
/// search radius of the linestring point, which was found by the
/// projection of the linestring point upon the closest edges within this radius.
pub struct LayerGenerator<'a, Emmis, Trans, E, M>
where
    M: Metadata,
    E: Entry,
    Emmis: EmissionStrategy,
    Trans: TransitionStrategy<E, M>,
{
    /// The maximum distance by which the generator will search for nodes,
    /// allowing it to find edges which may be comprised of distant nodes.
    ///
    /// This is a square-radius search, so may pick up nodes outside this
    /// distance as the edge may exist at the square-boundary, beyond the
    /// radial-boundary.
    pub search_distance: f64,

    /// The costing heuristics required to generate the layers.
    ///
    /// This is required as a caching technique since the costs for a candidate
    /// need only be calculated once.
    pub heuristics: &'a CostingStrategies<Emmis, Trans, E, M>,

    /// The routing map used to pull candidates from, and provide layout context.
    map: &'a Graph<E, M>,
}

struct PartiallyGeneratedCandidate<E: Entry> {
    candidate: Candidate<E>,
    candidate_ref: CandidateRef,
    layer_id: usize,
}

impl<'a, Emmis, Trans, E, M> LayerGenerator<'a, Emmis, Trans, E, M>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
    Trans: TransitionStrategy<E, M> + Send + Sync,
{
    /// Creates a [`LayerGenerator`] from a map and costing heuristics.
    pub fn new(
        map: &'a Graph<E, M>,
        heuristics: &'a CostingStrategies<Emmis, Trans, E, M>,
    ) -> Self {
        LayerGenerator {
            map,
            heuristics,

            search_distance: DEFAULT_SEARCH_DISTANCE,
        }
    }

    fn generate_candidates(
        &self,
        layer_id: usize,
        origin: &Point,
    ) -> impl IntoParallelIterator<Item = PartiallyGeneratedCandidate<E>> {
        self.map
            // We'll do a best-effort search (square) radius
            .scan_nodes_projected(origin, self.search_distance)
            .enumerate()
            .par_bridge()
            // Find the distance to the center point
            .map(|(index, (point, edge))| (index, point, edge, Haversine.distance(point, *origin)))
            // Get the index for each
            // And calculate the emission costs of each of these points
            .map(move |(node_id, position, edge, distance)| {
                let location = CandidateLocation { layer_id, node_id };

                // We have the actual projected position, and it's associated edge.
                // Therefore, we can use the Emission costing function to calculate
                // the associated emission cost of this candidate.
                let emission = self
                    .heuristics
                    .emission(EmissionContext::new(&position, origin, distance));

                let candidate = Candidate::new(edge.thin(), position, emission, location);
                let candidate_reference = CandidateRef::new(emission);

                (candidate, candidate_reference)
            })
            .map(
                move |(candidate, candidate_ref): (Candidate<E>, CandidateRef)| {
                    PartiallyGeneratedCandidate {
                        candidate,
                        candidate_ref,
                        layer_id,
                    }
                },
            )
    }

    fn default_data() -> (
        PetGraph<CandidateRef, CandidateEdge, Directed>,
        scc::HashMap<CandidateId, Candidate<E>>,
        HashMap<usize, Layer>,
    ) {
        (
            PetGraph::<CandidateRef, CandidateEdge, Directed>::default(),
            scc::HashMap::<CandidateId, Candidate<E>>::default(),
            HashMap::<usize, Layer>::default(),
        )
    }

    /// Utilises the configured search and filter distances to produce
    /// the candidates and layers required to match the initial input.
    pub fn with_points(&self, input: &[Point]) -> (Layers, Candidates<E>) {
        // In parallel, create each layer, and collect into a single structure.
        let (graph, lookup, layers) = input
            .into_par_iter()
            .enumerate()
            .flat_map(|(i, o)| self.generate_candidates(i, o))
            .collect::<Vec<PartiallyGeneratedCandidate<E>>>()
            .into_iter()
            .fold(
                Self::default_data(),
                |(mut graph, map, mut set),
                 PartiallyGeneratedCandidate {
                     candidate,
                     candidate_ref,
                     layer_id,
                 }| {
                    let node_index: CandidateId = graph.add_node(candidate_ref);
                    let _ = map.insert(node_index, candidate);

                    set.entry(layer_id)
                        .and_modify(|layer| {
                            layer.nodes.push(node_index);
                        })
                        .or_insert(Layer {
                            nodes: vec![node_index],
                            origin: input[layer_id],
                        });

                    (graph, map, set)
                },
            );

        let as_vec = Self::hashmap_to_vec(layers);

        (Layers { layers: as_vec }, Candidates::new(graph, lookup))
    }

    fn hashmap_to_vec<T>(mut map: HashMap<usize, T>) -> Vec<T> {
        map.into_iter()
            .sorted_by_key(|(i, _)| *i)
            .map(|(_, v)| v)
            .collect()
    }
}
