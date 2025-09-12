pub mod impls;

use crate::Candidates;
use crate::definition::Layers;
use geo::Point;
pub use impls::*;
use routers_codec::Entry;

pub trait LayerGeneration<E: Entry> {
    /// Generate consumes self and returns a pairing of the generated layers,
    /// and the candidate collection from which the layers will reference.
    fn generate(self, input: &[Point]) -> (Layers, Candidates<E>);
}
