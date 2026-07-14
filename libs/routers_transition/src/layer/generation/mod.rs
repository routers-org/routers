pub mod impls;

use crate::Candidate;
use geo::Point;
pub use impls::*;
use rayon::prelude::*;
use routers_network::Entry;

/// Produces the candidates anchoring each trajectory point.
///
/// Candidate *identity* is positional and assigned by the
/// [`Matcher`](crate::Matcher) when a layer is pushed; a generator only decides
/// which candidates exist and in what stable order, plus their emission cost.
pub trait LayerGeneration<E: Entry>: Send + Sync {
    /// The candidates anchoring a single `point` as layer `layer`, in stable
    /// order.
    fn candidates(&self, point: &Point, layer: usize) -> Vec<Candidate<E>>;

    /// One candidate set per input point, generated in parallel, with layers
    /// numbered from `first_layer` (the number of layers already generated).
    fn generate(&self, input: &[Point], first_layer: usize) -> Vec<Vec<Candidate<E>>> {
        input
            .into_par_iter()
            .enumerate()
            .map(|(offset, origin)| self.candidates(origin, first_layer + offset))
            .collect()
    }
}
