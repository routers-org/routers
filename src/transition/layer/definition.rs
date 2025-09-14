use crate::{Candidate, CandidateId};
use geo::{MultiPoint, Point};
use rayon::prelude::*;
use routers_codec::Entry;
use scc::HashMap;

/// A layer within the transition graph.
///
/// This represents a set of candidate [nodes](#field.nodes),
/// and the [origin](#field.origin) point, from which they originate.
pub struct Layer {
    /// All the candidates detected within the layer, as
    /// positions the [origin](#field.origin) could be matched to.
    pub nodes: Vec<CandidateId>,

    /// The input position within the input to the transition solver.
    ///
    /// This position is consumed by the [`LayerGenerator`](LayerGenerator)
    /// to produce candidates for each layer, based on intrinsic location properties.
    pub origin: Point,
}

impl Layer {
    pub fn geometry<E: Entry>(
        &self,
        lookup: &HashMap<CandidateId, Candidate<E>>,
    ) -> MultiPoint<f64> {
        self.nodes
            .iter()
            .filter_map(|id| lookup.get(id))
            .map(|candidate| candidate.position)
            .collect::<MultiPoint<_>>()
    }
}

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

    pub fn geometry<E: Entry>(
        &self,
        lookup: &scc::HashMap<CandidateId, Candidate<E>>,
    ) -> MultiPoint<f64> {
        self.layers
            .iter()
            .flat_map(|layer| layer.geometry(lookup).0)
            .collect::<MultiPoint<_>>()
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
