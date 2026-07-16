use geo::Point;
use routers_network::Entry;
use routers_trellis::{LayerId, Path, Solved, Trellis, TrellisError};
use serde::{Deserialize, Serialize};

use crate::candidate::{Candidate, CandidateRef, CandidateStore};

/// The state of one logical match, owned by the caller and handed to a
/// [`Matcher`](crate::Matcher) as data arrives.
///
/// A `Trip` is pure data — origins, candidates, and the trellis (building or
/// [`Solved`]) — with no borrows, so it can be stored, inspected, and
/// serialized between ticks. Its invariants (candidates aligned one-to-one
/// with trellis layers, positional identity true by placement) hold by
/// construction: every field is private and only [`Matcher`](crate::Matcher)
/// operations mutate it.
///
/// [`LayerId`] indexes everything: `point(id)` is the input position that
/// created the layer, `layer(id)` its candidates, and the same id addresses
/// the trellis.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct Trip<E>
where
    E: Entry,
{
    origins: Vec<Point>,
    candidates: CandidateStore<E>,
    state: TripState,
}

/// Where the trip's trellis currently sits in the `Unsolved ⇄ Solved` cycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) enum TripState {
    /// No layer has arrived yet.
    #[default]
    Empty,
    /// Growing / weighable.
    Building(Trellis),
    /// Certified: the held path describes the held trellis.
    Solved(Solved),
}

impl<E> Trip<E>
where
    E: Entry,
{
    /// A trip with no positions yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of layers (one per accepted input position).
    pub fn layers(&self) -> usize {
        self.origins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.origins.is_empty()
    }

    /// The id of the most recent layer.
    pub fn last_id(&self) -> Option<LayerId> {
        (!self.is_empty()).then(|| LayerId(self.origins.len() as u32 - 1))
    }

    /// The id the next pushed layer will take.
    pub fn next_id(&self) -> LayerId {
        LayerId(self.origins.len() as u32)
    }

    /// The input position that created `layer`.
    pub fn point(&self, layer: LayerId) -> Option<Point> {
        self.origins.get(layer.index()).copied()
    }

    /// Every input position, in layer order.
    pub fn points(&self) -> &[Point] {
        &self.origins
    }

    /// The candidates of one layer, in node order.
    pub fn layer(&self, layer: LayerId) -> Option<&[Candidate<E>]> {
        self.candidates.layer(layer)
    }

    /// The [`Candidate`] behind a ref, if present.
    pub fn candidate(&self, r: &CandidateRef) -> Option<Candidate<E>> {
        self.candidates.candidate(r)
    }

    /// Every candidate considered so far.
    pub fn candidates(&self) -> &CandidateStore<E> {
        &self.candidates
    }

    /// The trellis grown so far (building or solved), if any layer has arrived.
    pub fn trellis(&self) -> Option<&Trellis> {
        match &self.state {
            TripState::Empty => None,
            TripState::Building(trellis) => Some(trellis),
            TripState::Solved(solved) => Some(solved.trellis()),
        }
    }

    /// The current minimum-cost path, when the trip is solved.
    pub fn path(&self) -> Option<&Path> {
        match &self.state {
            TripState::Solved(solved) => Some(solved.path()),
            _ => None,
        }
    }

    /// Whether the trip is currently solved (no pending data).
    pub fn is_solved(&self) -> bool {
        matches!(self.state, TripState::Solved(_))
    }

    // ---- crate-internal seams used by the `Matcher` lifecycle ----

    /// Append one layer: its origin, its candidates (identity is overwritten to
    /// be positionally true), and a trellis layer carrying the emission costs
    /// as node weights. A solved trip reopens through [`Solved::append`].
    pub(crate) fn push_layer(
        &mut self,
        origin: Point,
        mut candidates: Vec<Candidate<E>>,
    ) -> Result<LayerId, TrellisError> {
        let width = candidates.len() as u32;

        let (mut trellis, id) = match core::mem::take(&mut self.state) {
            TripState::Empty => (Trellis::new(vec![width])?, LayerId(0)),
            TripState::Building(mut trellis) => {
                let id = trellis.add_layer(width)?;
                (trellis, id)
            }
            TripState::Solved(solved) => solved.append(width).map_err(|(solved, e)| {
                self.state = TripState::Solved(solved);
                e
            })?,
        };

        // Identity is placement: stamp each candidate with where it landed.
        for (node, candidate) in candidates.iter_mut().enumerate() {
            candidate.location = CandidateRef::new(id, routers_trellis::NodeId(node as u32));
        }

        // Emission costs enter the trellis as node weights, clamped to its
        // weight ceiling.
        let emissions = candidates
            .iter()
            .map(|candidate| candidate.emission.min(routers_trellis::MAX_WEIGHT))
            .collect::<Vec<_>>();
        trellis.fill_nodes(id, &emissions)?;

        self.origins.push(origin);
        self.candidates.push_layer(candidates);
        self.state = TripState::Building(trellis);

        Ok(id)
    }

    /// Take the state out for a solve, leaving `Empty`; the matcher must put
    /// it back via [`restore`](Self::restore) on every path.
    pub(crate) fn take_state(&mut self) -> TripState {
        core::mem::take(&mut self.state)
    }

    pub(crate) fn restore(&mut self, state: TripState) {
        self.state = state;
    }

    /// Move the candidates out for the final collapse.
    pub(crate) fn into_parts(self) -> (CandidateStore<E>, TripState) {
        (self.candidates, self.state)
    }
}
