use crate::candidate::CandidateRef;
use crate::costing::{EmissionContext, EmissionStrategy};
use crate::r#match::DEFAULT_SEARCH_DISTANCE;
use crate::{candidate::Candidate, layer::generation::LayerGeneration};
use geo::{Distance, Haversine, Point};
use routers_network::Network;
use routers_trellis::{LayerId, NodeId};

/// Every edge within [`search_distance`](Self::search_distance) of a
/// point becomes a candidate, capped at [`max_candidates`](Self::max_candidates).
#[derive(Copy, Clone)]
pub struct StandardGenerator<'a, N, Emmis>
where
    N: Network + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
{
    /// The maximum distance by which the generator will search for nodes,
    /// in metres.
    pub search_distance: f64,

    /// Keep only this many candidates per layer, best emission first.
    pub max_candidates: Option<usize>,

    pub emission: &'a Emmis,

    map: &'a N,
}

impl<'a, N, Emmis> StandardGenerator<'a, N, Emmis>
where
    N: Network + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
{
    pub fn new(map: &'a N, emission: &'a Emmis) -> Self {
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

    pub fn with_max_candidates(mut self, max_candidates: Option<usize>) -> Self {
        self.max_candidates = max_candidates;
        self
    }
}

impl<Emmis, N> LayerGeneration<N::Entry> for StandardGenerator<'_, N, Emmis>
where
    N: Network + ?Sized,
    Emmis: EmissionStrategy + Send + Sync,
{
    fn candidates(&self, origin: &Point, layer: LayerId) -> Vec<Candidate<N::Entry>> {
        let mut scored: Vec<_> = self
            .map
            .nearest_nodes_projected(origin, self.search_distance)
            .map(|(position, edge)| {
                let distance = Haversine.distance(position, *origin);
                let emission = self.emission.cost(EmissionContext::new(
                    &position,
                    origin,
                    distance,
                    edge.weight,
                ));
                (position, edge, emission)
            })
            .collect();

        // Node ids index the layer, so renumber after the cut.
        if let Some(k) = self.max_candidates
            && scored.len() > k
        {
            scored.sort_by_key(|(_, _, emission)| *emission);
            scored.truncate(k);
        }

        scored
            .into_iter()
            .enumerate()
            .map(|(node, (position, edge, emission))| {
                let location = CandidateRef::new(layer, NodeId(node as u32));
                Candidate::new(edge.thin(), position, emission, location)
            })
            .collect()
    }
}
