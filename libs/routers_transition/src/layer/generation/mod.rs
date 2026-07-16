pub mod impls;

use crate::Candidate;
use geo::Point;
pub use impls::*;
use rayon::prelude::*;
use routers_network::Entry;
use routers_trellis::LayerId;

/// Produces the candidates anchoring each trajectory point.
///
/// Candidate *identity* is positional and assigned by the
/// [`Matcher`](crate::Matcher) when a layer is pushed; a generator only decides
/// which candidates exist and in what stable order, plus their emission cost.
pub trait LayerGeneration<E: Entry>: Send + Sync {
    /// The candidates anchoring a single `point` as layer `layer`, in stable
    /// order.
    fn candidates(&self, point: &Point, layer: LayerId) -> Vec<Candidate<E>>;

    /// Generates all candidates, one set per input point, starting from `first_layer`.
    fn generate(&self, input: &[Point], first_layer: LayerId) -> Vec<Vec<Candidate<E>>> {
        input
            .into_par_iter()
            .enumerate()
            .map(|(offset, origin)| self.candidates(origin, LayerId(first_layer.0 + offset as u32)))
            .collect()
    }

    /// Generates the candidates for all input points, starting from the first layer.
    fn generate_all(&self, input: &[Point]) -> Vec<Vec<Candidate<E>>> {
        self.generate(input, LayerId::first())
    }
}
