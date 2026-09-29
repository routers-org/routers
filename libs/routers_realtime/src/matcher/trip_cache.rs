//! Matcher: the per-process trip cache behind sticky solve requests.
//!
//! After a solve, the matcher keeps the resulting [`Trip`] under the revision
//! the owner will commit it at. The owner's next request for that vehicle can
//! then omit the trip, the largest part of a solve job. A cache entry is only
//! a hint: the rebuilt job must still reproduce the request's job id, so a
//! stale or foreign trip is detected and answered as a miss.

use routers_network::Entry;
use routers_transition::matcher::Trip;
use rustc_hash::FxBuildHasher;
use scc::HashCache;
use scc::hash_cache::Entry as CacheEntry;

use crate::event::VehicleId;
use crate::protocol::ids::Revision;
use crate::protocol::job::TripDigest;

/// Trips retained per matcher process by default: roughly 3–4 KiB each.
pub const DEFAULT_TRIP_CACHE_ENTRIES: usize = 16_384;

/// A bounded, LRU-evicting map from vehicle to its most recent solved trip.
pub struct TripCache<E: Entry> {
    trips: HashCache<VehicleId, (Revision, Trip<E>, TripDigest), FxBuildHasher>,
}

impl<E: Entry> TripCache<E> {
    /// A cache holding at most `capacity` vehicles' trips. The backing map has a
    /// small fixed minimum, so capacities below 64 behave as 64.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is zero or not a power of two, so the bound is exact.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        assert!(
            capacity.is_power_of_two(),
            "trip cache capacity must be a nonzero power of two"
        );
        Self {
            trips: HashCache::with_capacity_and_hasher(0, capacity, FxBuildHasher),
        }
    }

    /// Remember `trip` and its digest as `vehicle`'s state once `revision`
    /// commits, replacing any older entry for the vehicle. The digest lets a
    /// cached request be verified without re-serialising the trip.
    pub fn insert(
        &self,
        vehicle: VehicleId,
        revision: Revision,
        trip: Trip<E>,
        digest: TripDigest,
    ) {
        match self.trips.entry(vehicle) {
            CacheEntry::Occupied(mut occupied) => {
                occupied.put((revision, trip, digest));
            }
            CacheEntry::Vacant(vacant) => {
                let _ = vacant.put_entry((revision, trip, digest));
            }
        }
    }

    /// Take `vehicle`'s trip if it was cached for exactly `revision`.
    ///
    /// Taking rather than cloning is deliberate: the solve that consumes it
    /// inserts its successor, and a retried request always carries the full trip.
    pub fn take(&self, vehicle: VehicleId, revision: Revision) -> Option<(Trip<E>, TripDigest)> {
        self.trips
            .remove_if(&vehicle, |(cached, _, _)| *cached == revision)
            .map(|(_, (_, trip, digest))| (trip, digest))
    }

    /// How many vehicles currently have a cached trip.
    #[must_use]
    pub fn len(&self) -> usize {
        self.trips.len()
    }

    /// Whether no trip is cached.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use routers_network::mock::MockEntryId;

    use super::*;

    #[test]
    fn take_requires_the_exact_revision_and_consumes_the_entry() {
        let cache = TripCache::<MockEntryId>::new(4);
        cache.insert(VehicleId(1), Revision(10), Trip::new(), TripDigest(10));

        assert!(
            cache.take(VehicleId(1), Revision(9)).is_none(),
            "older base"
        );
        assert!(
            cache.take(VehicleId(2), Revision(10)).is_none(),
            "other vehicle"
        );
        assert!(cache.take(VehicleId(1), Revision(10)).is_some());
        assert!(cache.take(VehicleId(1), Revision(10)).is_none(), "consumed");
    }

    #[test]
    fn insert_replaces_a_vehicles_older_trip() {
        let cache = TripCache::<MockEntryId>::new(4);
        cache.insert(VehicleId(1), Revision(10), Trip::new(), TripDigest(10));
        cache.insert(VehicleId(1), Revision(11), Trip::new(), TripDigest(11));

        assert_eq!(cache.len(), 1);
        assert!(cache.take(VehicleId(1), Revision(10)).is_none());
        assert!(cache.take(VehicleId(1), Revision(11)).is_some());
    }

    #[test]
    fn the_capacity_bounds_retained_vehicles() {
        let cache = TripCache::<MockEntryId>::new(1_024);
        for vehicle in 0..10_000 {
            cache.insert(VehicleId(vehicle), Revision(1), Trip::new(), TripDigest(1));
        }
        assert!(cache.len() <= 1_024, "retained {} trips", cache.len());
    }

    #[test]
    #[should_panic(expected = "power of two")]
    fn rejects_a_rounded_capacity() {
        let _ = TripCache::<MockEntryId>::new(12);
    }
}
