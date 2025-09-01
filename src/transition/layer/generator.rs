use crate::transition::*;
use crate::{Graph, Scan};

use geo::{Distance, Haversine, Point};
use itertools::Itertools;
use measure_time::debug_time;
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
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

const DEFAULT_SEARCH_DISTANCE: f64 = 100.; // 100m
const DEFAULT_FILTER_DISTANCE: f64 = 50.; // 50m

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
    pub search_distance: f64,

    /// The maximum distance by which matched candidates will be found,
    /// this directly minimises the cost to compute since it impacts the
    /// quantity of candidates found.
    ///
    /// A high search distance may take longer to compute but will give
    /// more accurate candidates as it can find edges who's comprising nodes
    /// are far apart.
    pub filter_distance: f64,

    /// The costing heuristics required to generate the layers.
    ///
    /// This is required as a caching technique since the costs for a candidate
    /// need only be calculated once.
    pub heuristics: &'a CostingStrategies<Emmis, Trans, E, M>,

    /// The routing map used to pull candidates from, and provide layout context.
    map: &'a Graph<E, M>,
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
            filter_distance: DEFAULT_FILTER_DISTANCE,
        }
    }

    /// Utilises the configured search and filter distances to produce
    /// the candidates and layers required to match the initial input.
    pub fn with_points(&self, input: &[Point]) -> (Layers, Candidates<E>) {
        let candidates = Candidates::default();

        // In parallel, create each layer, and collect into a single structure.
        let layers = input
            .into_par_iter()
            .enumerate()
            .map(|(layer_id, origin)| {
                debug_time!("{layer_id}: individual layer generation (!!)"); // 0.1 - 5.0ms

                // Generate an individual layer
                // Function takes about 10ms to compute.
                let nodes = {
                    debug_time!("{layer_id}: gen all");

                    self.map
                        // We'll do a best-effort search (square) radius
                        .scan_nodes_projected(origin, self.search_distance)
                        .filter_map(|(point, edge)| {
                            let distance = Haversine.distance(point, *origin);

                            if distance < self.filter_distance {
                                Some((point, edge, distance))
                            } else {
                                None
                            }
                        })
                        .sorted_by(|(_, _, a), (_, _, b)| a.total_cmp(b))
                        .take(25)
                        .enumerate()
                        .map(|(node_id, (position, edge, distance))| {
                            // We have the actual projected position, and it's associated edge.
                            // Therefore, we can use the Emission costing function to calculate
                            // the associated emission cost of this candidate.
                            let emission = self
                                .heuristics
                                .emission(EmissionContext::new(&position, origin, distance));

                            let location = CandidateLocation { layer_id, node_id };
                            let candidate =
                                Candidate::new(edge.thin(), position, emission, location);

                            let candidate_reference = CandidateRef::new(emission);
                            (candidate, candidate_reference)
                        })
                        .collect::<Vec<_>>()
                };

                // Inner-Scope for the graph, dropped on close.
                // Note: Contention here is negligible, runtime = free.
                let nodes = {
                    let mut graph = candidates.graph.write().unwrap();
                    nodes
                        .into_iter()
                        .map(|(candidate, candidate_ref)| {
                            let node_index = graph.add_node(candidate_ref);
                            let _ = candidates.lookup.insert(node_index, candidate);

                            node_index as CandidateId
                        })
                        .collect::<Vec<CandidateId>>()
                };

                Layer {
                    nodes,
                    origin: *origin,
                }
            })
            .collect::<Layers>();

        (layers, candidates)
    }
}
