use crate::context::Position;

/// Given an iterator of `(shard_id, position)` pairs in **newest-to-oldest** order,
/// returns up to `max_points` positions that are no older than `max_age_ms` before
/// `current_ts`, from at most two distinct shard zones, in oldest-first order.
///
/// Age filtering only applies when both `current_ts` and the entry's `timestamp_ms`
/// are non-zero. Entries are newest-first, so the first entry that falls outside the
/// age window terminates the scan — everything behind it is older still.
///
/// The shard-zone limit drops positions from a third or older zone because the target
/// pod has no map data for them. The point cap prevents excessively long windows for
/// vehicles that stay within one shard for a long time.
pub fn filter_history<S: Clone + PartialEq>(
    entries: impl Iterator<Item = (S, Position)>,
    max_points: usize,
    current_ts: u64,
    max_age_ms: u64,
) -> Vec<Position> {
    let mut seen: Vec<S> = Vec::new();
    let mut collected: Vec<Position> = Vec::new();

    for (shard, pos) in entries {
        if collected.len() == max_points {
            break;
        }
        // Skip entries newer than the current event. They ended up in the stream
        // ahead of us due to a concurrent orchestrator or a same-vehicle batch.
        // Use `continue` not `break` — the stream is insertion-ordered, not
        // GPS-time-ordered, so older entries can follow newer ones.
        if current_ts > 0 && pos.timestamp_ms > 0 && pos.timestamp_ms > current_ts {
            continue;
        }
        // Stop once an entry is outside the age window; everything after is older.
        if current_ts > 0 && pos.timestamp_ms > 0
            && current_ts.saturating_sub(pos.timestamp_ms) > max_age_ms
        {
            break;
        }
        if !seen.contains(&shard) {
            if seen.len() == 2 {
                break;
            }
            seen.push(shard);
        }
        collected.push(pos);
    }

    // Reverse from newest-first to oldest-first, then sort by GPS timestamp so
    // out-of-order insertions don't produce a zigzag linestring for the HMM.
    collected.reverse();
    collected.sort_by_key(|p| p.timestamp_ms);
    collected
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Point;

    #[derive(Debug, Clone, PartialEq)]
    struct FakeShard(u8);

    fn pos(ts: u64) -> Position {
        Position {
            coord: Point::new(0.0, 0.0),
            timestamp_ms: ts,
            resolved_at_ms: 0,
        }
    }

    #[test]
    fn keeps_two_shard_zones() {
        let entries = vec![
            (FakeShard(2), pos(4)),
            (FakeShard(2), pos(3)),
            (FakeShard(1), pos(2)),
            (FakeShard(1), pos(1)),
        ];
        let result = filter_history(entries.into_iter(), 100, 0, u64::MAX);
        assert_eq!(result.len(), 4);
        assert_eq!(result[0].timestamp_ms, 1);
        assert_eq!(result[3].timestamp_ms, 4);
    }

    #[test]
    fn drops_third_zone() {
        let entries = vec![
            (FakeShard(3), pos(5)),
            (FakeShard(2), pos(4)),
            (FakeShard(1), pos(3)),
            (FakeShard(1), pos(2)),
        ];
        let result = filter_history(entries.into_iter(), 100, 0, u64::MAX);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp_ms, 4);
        assert_eq!(result[1].timestamp_ms, 5);
    }

    #[test]
    fn single_zone() {
        let entries = vec![(FakeShard(1), pos(2)), (FakeShard(1), pos(1))];
        let result = filter_history(entries.into_iter(), 100, 0, u64::MAX);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp_ms, 1);
        assert_eq!(result[1].timestamp_ms, 2);
    }

    #[test]
    fn empty_yields_empty() {
        let result = filter_history::<FakeShard>(std::iter::empty(), 100, 0, u64::MAX);
        assert!(result.is_empty());
    }

    #[test]
    fn max_points_caps_single_zone() {
        // 20 entries newest-to-oldest: timestamps 19 down to 0
        let entries: Vec<_> = (0u64..20).rev().map(|i| (FakeShard(1), pos(i))).collect();
        let result = filter_history(entries.into_iter(), 5, 0, u64::MAX);
        assert_eq!(result.len(), 5);
        // collected newest-first: [19,18,17,16,15], reversed to oldest-first
        assert_eq!(result[0].timestamp_ms, 15);
        assert_eq!(result[4].timestamp_ms, 19);
    }

    #[test]
    fn age_filter_drops_stale_entries() {
        // current_ts = 1000ms, max_age = 300ms → keep entries with ts >= 700
        let entries = vec![
            (FakeShard(1), pos(950)),
            (FakeShard(1), pos(800)),
            (FakeShard(1), pos(600)), // too old — stops here
            (FakeShard(1), pos(400)),
        ];
        let result = filter_history(entries.into_iter(), 100, 1000, 300);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp_ms, 800);
        assert_eq!(result[1].timestamp_ms, 950);
    }

    #[test]
    fn skips_future_entries_without_breaking() {
        // A later event (ts=200) got inserted into the stream before the current
        // event (ts=100) processed its XREVRANGE. The future entry must be skipped
        // (not cause a break), and the remaining older entries must still be returned.
        let entries = vec![
            (FakeShard(1), pos(200)), // future — skip, not break
            (FakeShard(1), pos(100)), // current_ts == 100, included as history
            (FakeShard(1), pos(80)),
            (FakeShard(1), pos(60)),
        ];
        let result = filter_history(entries.into_iter(), 100, 100, 300);
        // ts=200 must be excluded; ts=100, 80, 60 must be present in oldest-first order
        assert_eq!(result.iter().map(|p| p.timestamp_ms).collect::<Vec<_>>(), vec![60, 80, 100]);
    }

    #[test]
    fn sorts_out_of_order_insertions() {
        // Stream insertion order doesn't match GPS timestamp order.
        let entries = vec![
            (FakeShard(1), pos(100)),
            (FakeShard(1), pos(95)),
            (FakeShard(1), pos(98)), // inserted out of order
            (FakeShard(1), pos(90)),
        ];
        let result = filter_history(entries.into_iter(), 100, 100, 300);
        assert_eq!(result.iter().map(|p| p.timestamp_ms).collect::<Vec<_>>(), vec![90, 95, 98, 100]);
    }

    #[test]
    fn age_filter_skipped_when_ts_zero() {
        // current_ts = 0 → age filter disabled, all entries kept
        let entries = vec![
            (FakeShard(1), pos(0)),
            (FakeShard(1), pos(0)),
            (FakeShard(1), pos(0)),
        ];
        let result = filter_history(entries.into_iter(), 100, 0, 300);
        assert_eq!(result.len(), 3);
    }
}
