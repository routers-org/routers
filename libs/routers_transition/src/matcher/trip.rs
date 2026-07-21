use geo::Point;
use routers_network::Entry;
use routers_trellis::{LayerId, Path, Solved, Trellis, TrellisError, ViterbiSolver};
use serde::{Deserialize, Serialize};

use crate::candidate::{Candidate, CandidateRef, CandidateStore};

/// The state of a match, ownership and responsibility lies with the caller.
///
/// The trick is this structure can be stored, inspected, and serialized
/// in and between loops, making it easy to build up a match state incrementally,
/// or in real-time.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub struct Trip<E>
where
    E: Entry,
{
    origins: Vec<Point>,
    candidates: CandidateStore<E>,
    pub state: TripState,
}

/// Where the trip's trellis currently sits in the `Unsolved ⇄ Solved` cycle.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum TripState {
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

    /// The most recent layer the solution has committed to: the matched path's
    /// prefix up to here cannot change no matter what positions follow, so a
    /// streaming consumer may emit it once and never revise it. `None` while
    /// the live hypotheses have not yet converged (or the trip is unsolved).
    ///
    /// The committed input positions are `points()[..=layer]`; the anchor
    /// candidate is `path()`'s node at `layer`. See `COALESCENCE.md` in
    /// `routers_trellis` for why the prefix is immutable.
    pub fn stable_upto(&self) -> Option<LayerId> {
        self.trellis().and_then(|t| ViterbiSolver::new().coalescence(t))
    }

    /// Drop the committed prefix, keeping only the volatile tail seeded by its
    /// anchor: window to the [`stable_upto`](Self::stable_upto) layer and pin
    /// that layer to its single anchor node, so continuing the stream re-solves
    /// only the still-changing suffix — and can never contradict the prefix
    /// already committed (see `COALESCENCE.md` §4.2). Returns the committed
    /// layer in the pre-trim numbering, or `None` if nothing has converged.
    ///
    /// A commit at layer 0 is a no-op: the start anchors the window already.
    pub fn commit(&mut self) -> Option<LayerId> {
        let layer = self.stable_upto()?;
        if layer.index() == 0 {
            return Some(layer);
        }
        let anchor = self.path()?.nodes.get(layer.index()).copied()?;

        self.tail(self.layers() - layer.index());
        self.candidates.pin_first(anchor);
        if let TripState::Building(trellis) = &mut self.state {
            trellis
                .pin_first(anchor)
                .expect("anchor indexes the retained first layer");
        }
        Some(layer)
    }

    /// Keep only the last `n` layers, discarding everything older.
    ///
    /// This is the windowing primitive for a long-running stream: the trellis
    /// is cut with [`Trellis::partition`] semantics, so the surviving interior
    /// boundaries stay *resolved* — the next [`solve`](crate::Matcher::solve)
    /// re-weighs nothing and only re-runs the µs-scale DP pass. A solved
    /// certificate cannot describe a shorter trellis, so a `Solved` trip
    /// reopens to `Building`.
    ///
    /// `n >= layers` is a no-op; `n == 0` empties the trip.
    pub fn tail(&mut self, n: usize) {
        let len = self.layers();
        if n >= len {
            return;
        }
        if n == 0 {
            *self = Self::new();
            return;
        }

        self.origins.drain(..len - n);
        self.candidates.tail(n);
        self.state = match core::mem::take(&mut self.state) {
            TripState::Empty => TripState::Empty,
            TripState::Building(trellis) => TripState::Building(
                trellis
                    .last(n)
                    .expect("trellis mirrors origins, so 0 < n < layers"),
            ),
            TripState::Solved(solved) => TripState::Building(
                solved
                    .trellis()
                    .last(n)
                    .expect("trellis mirrors origins, so 0 < n < layers"),
            ),
        };
    }

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
