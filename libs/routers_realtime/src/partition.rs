//! The fleet-wide stable hashing contract.
//!
//! Everything here is wire law, not implementation detail: producers in any
//! language derive a vehicle's partition, and every binary in the fleet must
//! agree with them bit-for-bit, across processes, releases, and rewrites.
//! Nothing in this module may change without a coordinated id-space
//! migration.

use core::fmt;
use core::ops::RangeInclusive;

use crate::event::VehicleId;

/// How many partitions the vehicle id space divides into. Fixed: subjects,
/// consumer names, and partition-to-pod assignment are all derived from it.
pub const PARTITIONS: u64 = 1024;

/// The durable-consumer shard number. It is deliberately distinct from a
/// partition: a shard is only a broker/control-plane grouping and never a
/// vehicle-routing decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ShardId(u16);

impl ShardId {
    /// The stable numeric shard identifier used in durable names.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl fmt::Display for ShardId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// A validated, fixed count of contiguous consumer shards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardCount(u16);

impl ShardCount {
    /// Validate a fleet-wide shard count. A divisor keeps every shard equally
    /// sized and makes a partition range's ownership unambiguous.
    pub fn new(count: u16) -> Result<Self, String> {
        let count = u64::from(count);
        if count == 0 || count > PARTITIONS || !PARTITIONS.is_multiple_of(count) {
            return Err(format!(
                "shards must be a non-zero divisor of {PARTITIONS}, got {count}"
            ));
        }
        Ok(Self(count as u16))
    }

    /// Number of consumer shards.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// Partitions represented by one shard.
    #[must_use]
    pub const fn width(self) -> u16 {
        (PARTITIONS as u16) / self.0
    }

    /// The shard that owns `partition`.
    #[must_use]
    pub const fn shard_of(self, partition: u16) -> ShardId {
        ShardId(partition / self.width())
    }

    /// Resolve a numeric shard identifier within this layout.
    #[must_use]
    pub const fn shard(self, number: u16) -> Option<ShardId> {
        if number < self.0 {
            Some(ShardId(number))
        } else {
            None
        }
    }

    /// The complete, contiguous partition range for `shard`.
    #[must_use]
    pub const fn partitions(self, shard: ShardId) -> RangeInclusive<u16> {
        let start = shard.0 * self.width();
        start..=start + self.width() - 1
    }

    /// Validate the raw-stream layout. Requiring both layouts to divide the
    /// fixed partition space means a raw shard is always wholly on one stream.
    pub fn validate_streams(self, streams: u64) -> Result<(), String> {
        if streams == 0 || streams > PARTITIONS || !PARTITIONS.is_multiple_of(streams) {
            return Err(format!(
                "streams must be a non-zero divisor of {PARTITIONS}, got {streams}"
            ));
        }
        if u64::from(self.0) % streams != 0 {
            return Err(format!(
                "shards ({}) must divide evenly across streams ({streams})",
                self.0
            ));
        }
        Ok(())
    }

    /// Every shard wholly contained in an owned partition range. Refusing a
    /// partial shard prevents two replicas from attaching to one durable.
    pub fn owned_shards(
        self,
        range: &RangeInclusive<u64>,
    ) -> Result<RangeInclusive<ShardId>, String> {
        let start = *range.start();
        let end = *range.end();
        if end >= PARTITIONS {
            return Err(format!("partition {end} outside 0..{PARTITIONS}"));
        }
        let width = u64::from(self.width());
        if !start.is_multiple_of(width) || !(end + 1).is_multiple_of(width) {
            return Err(format!(
                "owned partitions {start}-{end} split a shard of {width}; align replica ownership to shard boundaries"
            ));
        }
        Ok(ShardId((start / width) as u16)..=ShardId((end / width) as u16))
    }
}

/// FNV-1a 64. Stable by construction, which `DefaultHasher` is not: its
/// algorithm may change between Rust releases, and the fleet has to agree
/// on every placement this feeds.
pub fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// splitmix64 finaliser. FNV-1a avalanches poorly on its own — worst in the
/// low bits, which are exactly what a modulo reads — so every consumer of a
/// stable hash mixes before comparing or reducing.
pub fn mix(mut x: u64) -> u64 {
    x ^= x >> 30;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 27;
    x = x.wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The vehicle's partition: `splitmix64(id) % PARTITIONS`.
///
/// This is the cross-language producer contract. The id itself is already a
/// hash (FNV-1a 64 of the upstream string; see `realtime/v1/event.proto`),
/// so one mix pass repairs its low-bit weakness and the modulo is safe.
pub fn partition_of(vehicle: VehicleId) -> u64 {
    mix(vehicle.0) % PARTITIONS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Externally computed vectors. These pin the algorithm itself: a
    /// transcription slip in either constant still spreads keys evenly, and
    /// only a known answer catches it. `fnv1a(b"a")` is the published FNV-1a
    /// 64 test vector.
    #[test]
    fn matches_reference_vectors() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a(b"vehicle-42"), 0xf4dc_ea25_6ede_2c6c);

        assert_eq!(mix(0), 0);
        assert_eq!(mix(1), 0x5692_161d_100b_05e5);
        assert_eq!(mix(0xdead_beef), 0x4e06_2702_ec92_9eea);
        assert_eq!(mix(u64::MAX), 0xb4d0_55fc_f2cb_bd7b);

        assert_eq!(partition_of(VehicleId(1)), 485);
        assert_eq!(partition_of(VehicleId(0xdead_beef)), 746);
        assert_eq!(partition_of(VehicleId(u64::MAX)), 379);
    }

    /// Sequential ids are the pathological case for a bare modulo: without
    /// the mix they would fill partitions 0..n and leave the rest empty.
    #[test]
    fn sequential_ids_spread_across_partitions() {
        let per_partition = 100;
        let mut counts = vec![0usize; PARTITIONS as usize];

        for id in 0..(PARTITIONS * per_partition) {
            counts[partition_of(VehicleId(id)) as usize] += 1;
        }

        for (partition, count) in counts.iter().enumerate() {
            assert!(
                (25..400).contains(count),
                "partition {partition} took {count} of an expected ~{per_partition}"
            );
        }
    }

    #[test]
    fn shards_route_deterministically_and_cover_every_partition_once() {
        let shards = ShardCount::new(64).unwrap();
        let mut seen = [0_u8; PARTITIONS as usize];
        for number in 0..shards.get() {
            let shard = ShardId(number);
            for partition in shards.partitions(shard) {
                assert_eq!(shards.shard_of(partition), shard);
                seen[usize::from(partition)] += 1;
            }
        }
        assert!(seen.iter().all(|count| *count == 1));
    }

    #[test]
    fn shard_and_stream_layout_rejects_ambiguous_ownership() {
        assert!(ShardCount::new(63).is_err());
        let shards = ShardCount::new(64).unwrap();
        assert!(shards.validate_streams(4).is_ok());
        assert!(shards.validate_streams(3).is_err());
        assert!(shards.validate_streams(8).is_ok());
        assert!(shards.owned_shards(&(0..=255)).is_ok());
        assert!(shards.owned_shards(&(1..=255)).is_err());
        assert_eq!(shards.shard(63), Some(ShardId(63)));
        assert_eq!(shards.shard(64), None);
    }
}
