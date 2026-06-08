use crate::context::Position;

/// Given an iterator of `(shard_id, position)` pairs in **newest-to-oldest** order,
/// returns positions from the most recent two distinct shard zones in oldest-first order.
///
/// Positions from a third or older shard zone are dropped because the target pod has
/// no map data for them and they would produce a failed match.
pub fn filter_history<S: Clone + PartialEq>(
    entries: impl Iterator<Item = (S, Position)>,
) -> Vec<Position> {
    let mut seen: Vec<S> = Vec::new();
    let mut collected: Vec<Position> = Vec::new();

    for (shard, pos) in entries {
        if !seen.contains(&shard) {
            if seen.len() == 2 {
                break;
            }
            seen.push(shard);
        }
        collected.push(pos);
    }

    collected.reverse();
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
        let result = filter_history(entries.into_iter());
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
        let result = filter_history(entries.into_iter());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp_ms, 4);
        assert_eq!(result[1].timestamp_ms, 5);
    }

    #[test]
    fn single_zone() {
        let entries = vec![(FakeShard(1), pos(2)), (FakeShard(1), pos(1))];
        let result = filter_history(entries.into_iter());
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].timestamp_ms, 1);
        assert_eq!(result[1].timestamp_ms, 2);
    }

    #[test]
    fn empty_yields_empty() {
        let result = filter_history::<FakeShard>(std::iter::empty());
        assert!(result.is_empty());
    }
}
