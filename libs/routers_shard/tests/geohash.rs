//! Property-driven tests for [`GeohashStrategy`].

use geo::Point;
use routers_shard::{Geohash, GeohashStrategy, ShardingStrategy};

const SAMPLES: &[(f64, f64, &str)] = &[
    (151.2093, -33.8688, "Sydney"),
    (-118.2437, 34.0522, "Los Angeles"),
    (13.4050, 52.5200, "Berlin"),
    (139.6917, 35.6895, "Tokyo"),
    (0.0, 0.0, "Null Island"),
    (-0.1276, 51.5074, "London"),
    (-122.4194, 37.7749, "San Francisco"),
];

#[test]
fn locate_produces_string_of_requested_precision() {
    for precision in 1..=8 {
        let strategy = GeohashStrategy::with_precision(precision);
        let h = strategy.locate(Point::new(13.4, 52.5));
        assert_eq!(h.0.len(), precision as usize);
    }
}

#[test]
fn locate_is_deterministic() {
    let strategy = GeohashStrategy::with_precision(7);
    for &(x, y, name) in SAMPLES {
        let p = Point::new(x, y);
        assert_eq!(strategy.locate(p), strategy.locate(p), "non-deterministic at {name}");
    }
}

#[test]
fn locate_contains_its_own_point() {
    for precision in [1u8, 3, 5, 7, 9] {
        let strategy = GeohashStrategy::with_precision(precision);
        for &(x, y, name) in SAMPLES {
            let p = Point::new(x, y);
            let h = strategy.locate(p);
            assert!(
                strategy.contains(&h, p),
                "precision={precision} {name}: {h:?} did not contain {p:?}"
            );
        }
    }
}

#[test]
fn higher_precision_yields_smaller_bounds() {
    let mut prior_area = f64::INFINITY;
    for precision in 1..=8 {
        let strategy = GeohashStrategy::with_precision(precision);
        let h = strategy.locate(Point::new(13.4, 52.5));
        let r = strategy.bounds(&h);
        let area = (r.max().x - r.min().x) * (r.max().y - r.min().y);
        assert!(area < prior_area, "precision {precision}: {area} not < {prior_area}");
        prior_area = area;
    }
}

#[test]
fn known_geohash_prefix() {
    // The canonical geohash for the point (-5.6, 42.6) is "ezs42..." — the
    // value that appears across every reference implementation. Use it as a
    // floor on encoder correctness.
    let strategy = GeohashStrategy::with_precision(5);
    let h = strategy.locate(Point::new(-5.6, 42.6));
    assert_eq!(h.0, "ezs42", "expected ezs42, got {}", h.0);
}

#[test]
fn neighbours_share_precision_and_excludes_self() {
    let strategy = GeohashStrategy::with_precision(6);
    for &(x, y, name) in SAMPLES {
        let h = strategy.locate(Point::new(x, y));
        let neighbours = strategy.neighbours(&h);
        for n in &neighbours {
            assert_ne!(n, &h, "neighbour set must not contain self ({name})");
            assert_eq!(n.0.len(), h.0.len(), "neighbour precision mismatch ({name})");
        }
        let mut sorted = neighbours.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), neighbours.len(), "neighbours not unique ({name})");
    }
}

#[test]
fn inland_cells_have_eight_neighbours() {
    let strategy = GeohashStrategy::with_precision(6);
    let h = strategy.locate(Point::new(13.4050, 52.5200));
    assert_eq!(strategy.neighbours(&h).len(), 8);
}

#[test]
fn geohash_serde_roundtrip() {
    let strategy = GeohashStrategy::with_precision(7);
    for &(x, y, _) in SAMPLES {
        let h = strategy.locate(Point::new(x, y));
        let bytes = postcard::to_allocvec(&h).unwrap();
        let back: Geohash = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(h, back);
    }
}

#[test]
#[should_panic]
fn zero_precision_panics() {
    let _ = GeohashStrategy::with_precision(0);
}

#[test]
#[should_panic]
fn excessive_precision_panics() {
    let _ = GeohashStrategy::with_precision(20);
}
