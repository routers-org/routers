//! Candidate generation: how a trajectory point becomes a layer of road
//! candidates.
//!
//! A generator decides which candidates exist for a point, in what (stable)
//! order, and what their emission costs are — nothing more. Candidate
//! *identity* is positional and stamped by the [`Matcher`](crate::Matcher)
//! when the layer is pushed. [`StandardGenerator`] is the default: a radius
//! search projected onto the nearby edges. Implement [`LayerGeneration`] to
//! bring your own.

mod impls;

use cfg_if::cfg_if;
pub use impls::StandardGenerator;

use geo::Point;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use routers_network::Entry;
use routers_trellis::LayerId;

use crate::candidate::Candidate;

/// The longest input generated without the thread pool.
#[cfg(feature = "parallel")]
const SERIAL_POINTS: usize = 8;

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
        let layer = |(offset, origin): (usize, &Point)| {
            self.candidates(origin, LayerId(first_layer.0 + offset as u32))
        };

        cfg_if! {
            if #[cfg(feature = "parallel")] {
                if input.len() <= SERIAL_POINTS {
                    input.iter().enumerate().map(layer).collect()
                } else {
                    input.into_par_iter().enumerate().map(layer).collect()
                }
            } else {
                input.iter().enumerate().map(layer).collect()
            }
        }
    }

    /// Generates the candidates for all input points, starting from the first layer.
    fn generate_all(&self, input: &[Point]) -> Vec<Vec<Candidate<E>>> {
        self.generate(input, LayerId::first())
    }
}
