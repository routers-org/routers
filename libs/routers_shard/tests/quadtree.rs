//! Property-driven tests for [`QuadTreeStrategy`].

use geo::Point;
use routers_shard::{QuadKey, QuadTreeStrategy, ShardingStrategy};

/// A scattering of points across major continents — keeps tests deterministic
/// without requiring an RNG and exercises both hemispheres.
const SAMPLES: &[(f64, f64, &str)] = &[
    (151.2093, -33.8688, "Sydney"),
    (-118.2437, 34.0522, "Los Angeles"),
    (13.4050, 52.5200, "Berlin"),
    (139.6917, 35.6895, "Tokyo"),
    (-43.1729, -22.9068, "Rio de Janeiro"),
    (37.6173, 55.7558, "Moscow"),
    (28.0473, -26.2041, "Johannesburg"),
    (-58.3816, -34.6037, "Buenos Aires"),
    (174.7633, -36.8485, "Auckland"),
    (3.3792, 6.5244, "Lagos"),
    (0.0, 0.0, "Null Island"),
];

#[test]
fn locate_is_deterministic() {
    let strategy = QuadTreeStrategy::with_depth(12);
    for &(x, y, name) in SAMPLES {
        let p = Point::new(x, y);
        let a = strategy.locate(p);
        let b = strategy.locate(p);
        assert_eq!(a, b, "locate must be deterministic ({name})");
    }
}

#[test]
fn locate_contains_its_own_point() {
    for depth in [1u8, 4, 8, 12, 16] {
        let strategy = QuadTreeStrategy::with_depth(depth);
        for &(x, y, name) in SAMPLES {
            let p = Point::new(x, y);
            let k = strategy.locate(p);
            assert!(
                strategy.contains(&k, p),
                "depth={depth} {name}: {k:?} did not contain {p:?}"
            );
        }
    }
}

#[test]
fn bounds_shrink_with_depth() {
    let mut prior_area = f64::INFINITY;
    for depth in 0..=10 {
        let strategy = QuadTreeStrategy::with_depth(depth);
        let key = strategy.locate(Point::new(13.4, 52.5));
        let rect = strategy.bounds(&key);
        let area = (rect.max().x - rect.min().x) * (rect.max().y - rect.min().y);
        assert!(area < prior_area || depth == 0, "depth {depth}: area {area} not < {prior_area}");
        prior_area = area;
    }
}

#[test]
fn neighbours_share_depth_and_excludes_self() {
    let strategy = QuadTreeStrategy::with_depth(8);
    for &(x, y, name) in SAMPLES {
        let key = strategy.locate(Point::new(x, y));
        let neighbours = strategy.neighbours(&key);
        for n in &neighbours {
            assert_ne!(n, &key, "neighbour set must not contain self ({name})");
            assert_eq!(n.depth, key.depth, "neighbour depth mismatch ({name})");
        }
        // Distinctness.
        let mut sorted = neighbours.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), neighbours.len(), "neighbours not unique ({name})");
    }
}

#[test]
fn inland_cells_have_eight_neighbours() {
    // A small region near the equator and prime meridian is guaranteed to
    // sit well away from the world rectangle edges, so it should yield the
    // full 8-cell neighbourhood at any reasonable depth.
    let strategy = QuadTreeStrategy::with_depth(12);
    let key = strategy.locate(Point::new(13.4050, 52.5200));
    assert_eq!(strategy.neighbours(&key).len(), 8);
}

#[test]
fn edge_cells_have_fewer_neighbours() {
    // A pole has only southerly neighbours; the dateline-equator corner has
    // only easterly ones (we don't wrap). Both should be < 8.
    let strategy = QuadTreeStrategy::with_depth(8);
    let pole = strategy.locate(Point::new(0.0, 89.99));
    let dateline = strategy.locate(Point::new(179.99, 0.0));
    assert!(strategy.neighbours(&pole).len() < 8, "pole had {} neighbours", strategy.neighbours(&pole).len());
    assert!(strategy.neighbours(&dateline).len() < 8);
}

#[test]
fn neighbour_cells_abut_owned_cell() {
    let strategy = QuadTreeStrategy::with_depth(10);
    for &(x, y, name) in SAMPLES {
        let key = strategy.locate(Point::new(x, y));
        let bounds = strategy.bounds(&key);
        for n in strategy.neighbours(&key) {
            let nb = strategy.bounds(&n);
            let abuts_x = (nb.min().x - bounds.max().x).abs() < 1e-9
                || (nb.max().x - bounds.min().x).abs() < 1e-9;
            let abuts_y = (nb.min().y - bounds.max().y).abs() < 1e-9
                || (nb.max().y - bounds.min().y).abs() < 1e-9;
            let overlap_x = nb.min().x <= bounds.max().x && nb.max().x >= bounds.min().x;
            let overlap_y = nb.min().y <= bounds.max().y && nb.max().y >= bounds.min().y;
            assert!(
                (abuts_x && overlap_y) || (abuts_y && overlap_x) || (abuts_x && abuts_y),
                "{name}: neighbour {n:?} does not touch {key:?}"
            );
        }
    }
}

#[test]
fn root_has_full_world_bounds() {
    let strategy = QuadTreeStrategy::with_depth(0);
    let key = strategy.locate(Point::new(0.0, 0.0));
    assert_eq!(key.depth, 0);
    let bounds = strategy.bounds(&key);
    assert_eq!(bounds.min().x, -180.0);
    assert_eq!(bounds.max().x, 180.0);
    assert_eq!(bounds.min().y, -90.0);
    assert_eq!(bounds.max().y, 90.0);
}

#[test]
fn depth_one_quadrant_assignment() {
    // At depth 1 we expect exactly four cells, one per geographic hemisphere
    // pair. Verify the encoding matches the documented convention:
    //   0b00 SW, 0b01 SE, 0b10 NW, 0b11 NE
    let strategy = QuadTreeStrategy::with_depth(1);
    let sw = strategy.locate(Point::new(-90.0, -45.0));
    let se = strategy.locate(Point::new(90.0, -45.0));
    let nw = strategy.locate(Point::new(-90.0, 45.0));
    let ne = strategy.locate(Point::new(90.0, 45.0));
    assert_eq!(sw.bits & 0b11, 0b00);
    assert_eq!(se.bits & 0b11, 0b01);
    assert_eq!(nw.bits & 0b11, 0b10);
    assert_eq!(ne.bits & 0b11, 0b11);
    // All depth-1 keys must be among the four canonical IDs.
    for k in [sw, se, nw, ne] {
        assert_eq!(k.depth, 1);
        assert!(k.bits <= 0b11);
    }
}

#[test]
fn out_of_range_points_clamp() {
    let strategy = QuadTreeStrategy::with_depth(6);
    let inside = strategy.locate(Point::new(180.0, 90.0));
    let outside = strategy.locate(Point::new(1_000.0, 1_000.0));
    assert_eq!(inside, outside, "out-of-range points should clamp into the world rectangle");
}

#[test]
fn neighbours_at_different_depths_dont_share_ids() {
    let s1 = QuadTreeStrategy::with_depth(5);
    let s2 = QuadTreeStrategy::with_depth(10);
    let p = Point::new(13.4050, 52.5200);
    let k1 = s1.locate(p);
    let k2 = s2.locate(p);
    assert_ne!(k1.depth, k2.depth);
    // Bit layouts share a common prefix, but the depth tag ensures the IDs
    // are not equal.
    assert_ne!(k1, k2);
}

#[test]
fn quadkey_serde_roundtrip() {
    let strategy = QuadTreeStrategy::with_depth(12);
    for &(x, y, _) in SAMPLES {
        let key = strategy.locate(Point::new(x, y));
        let bytes = postcard::to_allocvec(&key).unwrap();
        let back: QuadKey = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(key, back);
    }
}

#[test]
fn quadkey_debug_renders_depth_and_path() {
    let strategy = QuadTreeStrategy::with_depth(3);
    let key = strategy.locate(Point::new(13.4, 52.5));
    let s = format!("{key:?}");
    assert!(s.contains("d3"), "debug format should include depth: {s}");
}

#[test]
#[should_panic]
fn excessive_depth_panics() {
    let _ = QuadTreeStrategy::with_depth(50);
}
