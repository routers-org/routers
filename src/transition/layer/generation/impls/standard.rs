use crate::definition::{Layer, Layers};
use crate::generation::LayerGeneration;
use crate::transition::*;
use crate::{Graph, Scan};
use geo::{Distance, Haversine, Point};
use itertools::Itertools;
use rayon::prelude::*;
use routers_codec::{Entry, Metadata};
use std::collections::HashMap;

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
    map: &'a Graph<E, M>,
}

struct PartiallyGeneratedCandidate<E: Entry> {
    candidate: Candidate<E>,
    candidate_ref: CandidateRef,
    layer_id: usize,
}

#[derive(Default)]
struct PartialLayerGeneration<E: Entry> {
    /// An unlocked candidate graph used to generate the candidates
    /// whilst in a write-only state.
    candidate_graph: OpenCandidateGraph,

    /// A concurrent hashmap used to lookup candidates by their ID.
    lookup: scc::HashMap<CandidateId, Candidate<E>>,

    /// A map of layers, with key being the layer index, and value
    /// being the layer itself.
    layers: HashMap<usize, Layer>,
}

impl<'a, E, M, Emmis> StandardGenerator<'a, E, M, Emmis>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
{
    /// Creates a [`StandardGenerator`] from a map and emission heuristic.
    pub fn new(map: &'a Graph<E, M>, emission: &'a Emmis, search_distance: f64) -> Self {
        StandardGenerator {
            map,
            emission,
            search_distance,
        }
    }

    /// Finds relevant candidates for a given point, and associated layer-id
    fn discover_candidates(
        &self,
        layer_id: usize,
        origin: &Point,
    ) -> impl IntoParallelIterator<Item = PartiallyGeneratedCandidate<E>> {
        self.map
            // We'll do a best-effort search (square) radius
            .scan_nodes_projected(origin, self.search_distance)
            // Get the index for each
            .enumerate()
            .par_bridge()
            // And calculate the emission costs of each of these points
            .map(move |(node_id, (position, edge))| {
                let location = CandidateLocation { layer_id, node_id };
                let distance = Haversine.distance(position, *origin);

                // We have the actual projected position, and it's associated edge.
                // Therefore, we can use the Emission costing function to calculate
                // the associated emission cost of this candidate.
                let emission = self
                    .emission
                    .cost(EmissionContext::new(&position, origin, distance));

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

    fn prepare_candidate(
        mut layer: PartialLayerGeneration<E>,
        candidate: &PartiallyGeneratedCandidate<E>,
        origin: &Point,
    ) -> PartialLayerGeneration<E> {
        // Insert the candidate into the graph, obtaining the assigned identifier
        let node_index: CandidateId = layer.candidate_graph.add_node(candidate.candidate_ref);
        // Insert this identifier into the lookup table, keyed to the associated candidate value
        let _ = layer.lookup.insert(node_index, candidate.candidate);

        // Insert this node into an existing layer, or create a new one if required.
        layer
            .layers
            .entry(candidate.layer_id)
            .and_modify(|layer| {
                layer.nodes.push(node_index);
            })
            .or_insert(Layer {
                nodes: vec![node_index],
                origin: *origin,
            });

        layer
    }
}

fn hashmap_to_vec<T>(map: HashMap<usize, T>) -> Vec<T> {
    map.into_iter()
        .sorted_by_key(|(i, _)| *i)
        .map(|(_, v)| v)
        .collect()
}

impl<Emmis, E, M> LayerGeneration<E> for StandardGenerator<'_, E, M, Emmis>
where
    E: Entry,
    M: Metadata,
    Emmis: EmissionStrategy + Send + Sync,
{
    fn generate(self, input: &[Point]) -> (Layers, Candidates<E>) {
        let fold_binding = |layer, candidate| {
            Self::prepare_candidate(layer, &candidate, &input[candidate.layer_id])
        };

        // In parallel, create each layer, and collect into a single structure.
        let PartialLayerGeneration {
            candidate_graph,
            lookup,
            layers,
        } = input
            .into_par_iter()
            .enumerate()
            .flat_map(|(i, o)| self.discover_candidates(i, o))
            .collect::<Vec<PartiallyGeneratedCandidate<E>>>()
            .into_iter()
            .fold(PartialLayerGeneration::<E>::default(), fold_binding);

        let layers = hashmap_to_vec(layers);
        let candidates = Candidates::new(candidate_graph, lookup);

        (Layers { layers }, candidates)
    }
}
