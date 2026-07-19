use geo::Point;
use routers_network::Entry;
use serde::{Deserialize, Serialize};

use crate::matcher::Trip;

/// How a persisted [`Trip`] relates to the authoritative history it is being
/// resumed against — the decision a streaming consumer needs before its next
/// [`solve`](crate::Matcher::solve).
///
/// A trip persisted between ticks can drift from the committed history: the
/// history window slides, stale points age out, and a teleporting vehicle has
/// everything before the jump discarded. [`reconcile`](Continuation::reconcile)
/// aligns the two and answers whether the trellis grown so far is still worth
/// keeping.
///
/// Serializable, so the reconciliation can happen away from the matcher: a
/// data-plane component holding the committed history (which can trim and
/// compare, but never generate a layer) reconciles and ships the continuation
/// to the compute node that owns a generator.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub enum Continuation<E>
where
    E: Entry,
{
    /// The trip agrees with the history: it has been trimmed to exactly the
    /// overlap (via [`Trip::tail`], so resolved boundaries survive), and only
    /// `fresh` — the history points the trip has not seen — need pushing.
    Resume { trip: Trip<E>, fresh: Vec<Point> },

    /// The trip contradicts the history (or there was none): its trellis
    /// describes points the history no longer stands behind, so it must be
    /// discarded and the whole history matched from scratch.
    Restart { fresh: Vec<Point> },
}

impl<E> Continuation<E>
where
    E: Entry,
{
    /// Reconcile a persisted trip with the committed `history`
    /// (chronological, oldest first).
    ///
    /// The overlap is the longest prefix of `history` that is also a suffix of
    /// the trip's origins. Anything the trip knows *before* the overlap has
    /// slid out of (or been cut from) the history, so it is trimmed away —
    /// which both honours teleport/gap cutoffs and keeps a long-running trip
    /// bounded to the history window. No overlap at all means the trellis
    /// describes a different past: restart.
    ///
    /// Points the trip skipped (e.g. unanchored pushes) simply reappear in
    /// `fresh`, since the overlap ends where the origins stop agreeing.
    pub fn reconcile(trip: Option<Trip<E>>, history: &[Point]) -> Self {
        let Some(mut trip) = trip else {
            return Self::Restart {
                fresh: history.to_vec(),
            };
        };

        let origins = trip.points();
        let bound = origins.len().min(history.len());
        let overlap = (0..=bound)
            .rev()
            .find(|&k| origins[origins.len() - k..] == history[..k])
            .unwrap_or(0);

        if overlap == 0 {
            return Self::Restart {
                fresh: history.to_vec(),
            };
        }

        trip.tail(overlap);
        Self::Resume {
            trip,
            fresh: history[overlap..].to_vec(),
        }
    }
}
