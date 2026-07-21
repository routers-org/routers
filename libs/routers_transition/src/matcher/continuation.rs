use geo::Point;
use routers_network::Entry;
use serde::{Deserialize, Serialize};

use crate::matcher::Trip;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(bound(serialize = "E: Serialize", deserialize = "E: Deserialize<'de>"))]
pub enum Continuation<E>
where
    E: Entry,
{
    /// The trip agrees with the history, and is resumable. The
    /// fresh points are those beyond the trip which are not yet
    /// matched against the history.
    Resume { trip: Trip<E>, fresh: Vec<Point> },

    /// The trip contradicts the history (or there was none), and
    /// must be restarted from scratch, the raw event history
    /// is given.
    Restart { fresh: Vec<Point> },
}

impl<E> Continuation<E>
where
    E: Entry,
{
    /// Reconcile a persisted trip with the committed `history` (chronological, oldest first).
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
