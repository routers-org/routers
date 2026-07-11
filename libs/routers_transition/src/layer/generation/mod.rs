pub mod impls;

use crate::definition::Layers;
use crate::{Candidate, Candidates};
use geo::Point;
pub use impls::*;
use routers_network::Entry;

pub trait LayerGeneration<E: Entry> {
    /// Generate consumes self and returns a pairing of the generated layers,
    /// and the candidate collection from which the layers will reference.
    fn generate(self, input: &[Point]) -> (Layers, Candidates<E>);

    /// The candidates anchoring a single `point` as layer `layer`, in stable
    /// order, with node ids sequential from zero. This is the streaming
    /// counterpart of [`generate`](LayerGeneration::generate): a realtime
    /// consumer receiving one position at a time appends each point's
    /// candidates as a new layer (see [`Transition::push`](crate::Transition)).
    fn candidates(&self, point: &Point, layer: usize) -> Vec<Candidate<E>>;
}
