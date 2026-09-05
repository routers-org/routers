//! Property-driven tests for [`S2Strategy`].
#![cfg(feature = "s2")]

use geo::{Point, Rect};
use routers_shard::{S2CellId, S2ParseError, S2Strategy, ShardingStrategy};

/// A scattering of points across major continents — keeps tests deterministic
/// without requiring an RNG and exercises all six cube faces.
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
    (178.4419, -18.1416, "Suva"),
    (-179.9, 65.0, "Chukotka"),
    (0.0, 89.9, "North Pole"),
    (0.0, -89.9, "South Pole"),
];

/// Latitude in degrees of the cube vertex `(1, 1, 1) / √3`, where faces 0,
/// 1 and 2 meet.
const CUBE_VERTEX_LAT: f64 = 35.264_389_682_754_654;

fn rect_contains(rect: &Rect, p: Point) -> bool {
    let (x, y) = p.x_y();
    x >= rect.min().x && x <= rect.max().x && y >= rect.min().y && y <= rect.max().y
}

/// Closed-rectangle intersection test that also accepts rectangles which
/// meet across the antimeridian (one ending at 180°, the other starting at
/// -180°).
fn rects_touch(a: &Rect, b: &Rect) -> bool {
    const EPS: f64 = 1e-9;
    let overlap_y = a.min().y <= b.max().y + EPS && b.min().y <= a.max().y + EPS;
    let overlap_x = a.min().x <= b.max().x + EPS && b.min().x <= a.max().x + EPS;
    let wrap_x =
        (a.max().x == 180.0 && b.min().x == -180.0) || (b.max().x == 180.0 && a.min().x == -180.0);
    overlap_y && (overlap_x || wrap_x)
}

#[test]
fn locate_is_deterministic() {
    let strategy = S2Strategy::with_level(12);
    for &(x, y, name) in SAMPLES {
        let p = Point::new(x, y);
        assert_eq!(
            strategy.locate(p),
            strategy.locate(p),
            "locate must be deterministic ({name})"
        );
    }
}

#[test]
fn locate_yields_requested_level() {
    for level in [0u8, 1, 4, 8, 12, 16, 24, 30] {
        let strategy = S2Strategy::with_level(level);
        for &(x, y, name) in SAMPLES {
            let id = strategy.locate(Point::new(x, y));
            assert_eq!(id.level(), level, "{name}: {id:?} is not at level {level}");
        }
    }
}

#[test]
fn locate_contains_its_own_point() {
    for level in [0u8, 1, 4, 8, 12, 16, 30] {
        let strategy = S2Strategy::with_level(level);
        for &(x, y, name) in SAMPLES {
            let p = Point::new(x, y);
            let id = strategy.locate(p);
            assert!(
                strategy.contains(&id, p),
                "level={level} {name}: {id:?} did not contain {p:?}"
            );
        }
    }
}

#[test]
fn contains_rejects_points_in_other_cells() {
    let strategy = S2Strategy::with_level(10);
    let berlin = strategy.locate(Point::new(13.4050, 52.5200));
    for &(x, y, name) in SAMPLES {
        if name == "Berlin" {
            continue;
        }
        assert!(
            !strategy.contains(&berlin, Point::new(x, y)),
            "{name} must not fall inside Berlin's cell"
        );
    }
}

#[test]
fn bounds_contain_located_point() {
    for level in [0u8, 1, 4, 8, 12, 16] {
        let strategy = S2Strategy::with_level(level);
        for &(x, y, name) in SAMPLES {
            let p = Point::new(x, y);
            let rect = strategy.bounds(&strategy.locate(p));
            assert!(
                rect_contains(&rect, p),
                "level={level} {name}: {rect:?} did not contain {p:?}"
            );
        }
    }
}

#[test]
fn bounds_are_conservative_over_neighbour_edges() {
    // Every point that `contains` admits must also be inside `bounds`. Probe
    // a fine grid over the bounding rectangle of a mid-latitude cell, where
    // the great-circle edges deviate most from a lat/lng rectangle.
    let strategy = S2Strategy::with_level(8);
    let id = strategy.locate(Point::new(13.4050, 52.5200));
    let rect = strategy.bounds(&id);
    let (w, h) = (rect.width(), rect.height());
    for i in 0..=40 {
        for j in 0..=40 {
            let p = Point::new(
                rect.min().x - 0.5 * w + (2.0 * w) * (i as f64 / 40.0),
                rect.min().y - 0.5 * h + (2.0 * h) * (j as f64 / 40.0),
            );
            if strategy.contains(&id, p) {
                assert!(rect_contains(&rect, p), "{p:?} in cell but outside bounds");
            }
        }
    }
}

#[test]
fn bounds_shrink_with_level() {
    // Berlin sits on the polar face 2. Its level-0 and level-1 cells both
    // touch the pole, so both span every longitude and share a bound; from
    // level 2 the cell leaves the pole and the bound shrinks at each level.
    let mut prior_area = f64::INFINITY;
    for level in 0..=12 {
        let strategy = S2Strategy::with_level(level);
        let id = strategy.locate(Point::new(13.4, 52.5));
        let rect = strategy.bounds(&id);
        let area = rect.width() * rect.height();
        if level >= 2 {
            assert!(
                area < prior_area,
                "level {level}: area {area} not < {prior_area}"
            );
        } else {
            assert!(area <= prior_area, "level {level}: area {area} grew");
        }
        prior_area = area;
    }
}

#[test]
fn level_zero_is_the_six_cube_faces() {
    // Face cells have ids `(face << 61) | (1 << 60)`, which tokenise to a
    // single hex digit. Each face is centred on one of the six axis
    // directions of the unit sphere.
    let strategy = S2Strategy::with_level(0);
    let faces = [
        (0.0, 0.0, 0u8, "1"),
        (90.0, 0.0, 1, "3"),
        (0.0, 90.0, 2, "5"),
        (180.0, 0.0, 3, "7"),
        (-90.0, 0.0, 4, "9"),
        (0.0, -90.0, 5, "b"),
    ];
    for (x, y, face, token) in faces {
        let id = strategy.locate(Point::new(x, y));
        assert_eq!(id.face(), face, "({x}, {y}) landed on the wrong face");
        assert_eq!(id.level(), 0);
        assert_eq!(id.to_string(), token);
    }
}

#[test]
fn neighbours_share_level_and_exclude_self() {
    let strategy = S2Strategy::with_level(8);
    for &(x, y, name) in SAMPLES {
        let id = strategy.locate(Point::new(x, y));
        let neighbours = strategy.neighbours(&id);
        for n in &neighbours {
            assert_ne!(n, &id, "neighbour set must not contain self ({name})");
            assert_eq!(n.level(), id.level(), "neighbour level mismatch ({name})");
        }
        let mut sorted = neighbours.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            neighbours.len(),
            "neighbours not unique ({name})"
        );
    }
}

#[test]
fn inland_cells_have_eight_neighbours() {
    let strategy = S2Strategy::with_level(12);
    let id = strategy.locate(Point::new(13.4050, 52.5200));
    assert_eq!(strategy.neighbours(&id).len(), 8);
}

#[test]
fn antimeridian_and_pole_cells_have_eight_neighbours() {
    // Unlike the planar strategies, S2 wraps: cells on the antimeridian and
    // at the poles still have a complete 8-cell neighbourhood.
    let strategy = S2Strategy::with_level(8);
    for (x, y) in [(179.99, 0.0), (-179.99, 45.0), (0.0, 89.99), (0.0, -89.99)] {
        let id = strategy.locate(Point::new(x, y));
        assert_eq!(strategy.neighbours(&id).len(), 8, "({x}, {y}) via {id:?}");
    }
}

#[test]
fn cube_vertex_cells_have_seven_neighbours() {
    // Only three faces meet at a cube vertex, so the three cells sharing it
    // each have one diagonal neighbour fewer.
    let strategy = S2Strategy::with_level(8);
    let delta = 1e-6;
    for (x, y) in [
        (45.0 - delta, CUBE_VERTEX_LAT - delta),
        (45.0 + delta, CUBE_VERTEX_LAT - delta),
        (45.0, CUBE_VERTEX_LAT + delta),
    ] {
        let id = strategy.locate(Point::new(x, y));
        assert_eq!(strategy.neighbours(&id).len(), 7, "({x}, {y}) via {id:?}");
    }
}

#[test]
fn neighbours_are_mutual() {
    let strategy = S2Strategy::with_level(8);
    for &(x, y, name) in SAMPLES {
        let id = strategy.locate(Point::new(x, y));
        for n in strategy.neighbours(&id) {
            assert!(
                strategy.neighbours(&n).contains(&id),
                "{name}: {n:?} does not list {id:?} as a neighbour"
            );
        }
    }
}

#[test]
fn neighbour_bounds_touch_owned_bounds() {
    let strategy = S2Strategy::with_level(10);
    for &(x, y, name) in SAMPLES {
        let id = strategy.locate(Point::new(x, y));
        let bounds = strategy.bounds(&id);
        for n in strategy.neighbours(&id) {
            let nb = strategy.bounds(&n);
            assert!(
                rects_touch(&bounds, &nb),
                "{name}: neighbour {n:?} bounds {nb:?} do not touch {bounds:?}"
            );
        }
    }
}

#[test]
fn antimeridian_cells_keep_tight_longitude_bounds() {
    // Cell edges run along the antimeridian rather than across it, so a cell
    // on either side is bounded by ±180 exactly, not by the full span.
    let strategy = S2Strategy::with_level(6);

    let east = strategy.bounds(&strategy.locate(Point::new(179.9, 10.0)));
    assert_eq!(east.max().x, 180.0);
    assert!(east.min().x > 170.0, "east bounds too loose: {east:?}");

    let west = strategy.bounds(&strategy.locate(Point::new(-179.9, 10.0)));
    assert_eq!(west.min().x, -180.0);
    assert!(west.max().x < -170.0, "west bounds too loose: {west:?}");
}

#[test]
fn face_three_and_polar_cells_span_all_longitudes() {
    // Face 3 is centred on the antimeridian and a polar cell surrounds the
    // pole, so neither has a meaningful longitude range.
    let face = S2Strategy::with_level(0);
    let rect = face.bounds(&face.locate(Point::new(180.0, 0.0)));
    assert_eq!((rect.min().x, rect.max().x), (-180.0, 180.0));

    let polar = S2Strategy::with_level(6);
    let rect = polar.bounds(&polar.locate(Point::new(0.0, 89.99)));
    assert_eq!((rect.min().x, rect.max().x), (-180.0, 180.0));
    assert_eq!(rect.max().y, 90.0);
}

#[test]
fn longitude_wraps_and_latitude_clamps() {
    let strategy = S2Strategy::with_level(10);
    assert_eq!(
        strategy.locate(Point::new(190.0, 10.0)),
        strategy.locate(Point::new(-170.0, 10.0)),
        "190° is the same meridian as -170°"
    );
    assert_eq!(
        strategy.locate(Point::new(10.0, 90.0)),
        strategy.locate(Point::new(10.0, 1_000.0)),
        "out-of-range latitude should clamp to the pole"
    );
}

#[test]
fn ids_at_different_levels_dont_collide() {
    let p = Point::new(13.4050, 52.5200);
    let a = S2Strategy::with_level(5).locate(p);
    let b = S2Strategy::with_level(10).locate(p);
    assert_ne!(a, b);
    assert_ne!(a.level(), b.level());
}

#[test]
fn token_roundtrip() {
    for level in [0u8, 3, 8, 15, 30] {
        let strategy = S2Strategy::with_level(level);
        for &(x, y, name) in SAMPLES {
            let id = strategy.locate(Point::new(x, y));
            let token = id.to_string();
            assert!(
                token.len() <= 16 && !token.ends_with('0'),
                "{name}: {token}"
            );
            let back: S2CellId = token.parse().unwrap();
            assert_eq!(id, back, "{name}: token {token} did not round-trip");
        }
    }
}

#[test]
fn invalid_tokens_are_rejected() {
    for bad in ["", "X", "zz", "+1", "0", "00000000000000000", "2"] {
        assert_eq!(
            bad.parse::<S2CellId>(),
            Err(S2ParseError::InvalidToken(bad.to_owned())),
            "{bad:?} should not parse"
        );
    }
    assert_eq!(S2CellId::from_raw(0), None);
    assert!(S2CellId::from_raw(1 << 60).is_some());
}

#[test]
fn serde_roundtrip() {
    let strategy = S2Strategy::with_level(12);
    for &(x, y, _) in SAMPLES {
        let id = strategy.locate(Point::new(x, y));
        let bytes = postcard::to_allocvec(&id).unwrap();
        let back: S2CellId = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(id, back);
    }
}

#[test]
fn debug_renders_level_and_token() {
    let strategy = S2Strategy::with_level(3);
    let id = strategy.locate(Point::new(13.4, 52.5));
    let s = format!("{id:?}");
    assert!(s.contains("l3"), "debug format should include level: {s}");
    assert!(
        s.contains(&id.to_string()),
        "debug format should include token: {s}"
    );
}

#[test]
#[should_panic]
fn excessive_level_panics() {
    let _ = S2Strategy::with_level(31);
}
