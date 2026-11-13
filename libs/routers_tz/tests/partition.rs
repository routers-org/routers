#[path = "../build/partition.rs"]
mod partition;
#[path = "../build/s2_cells.rs"]
mod s2_cells;

use geo::{Contains, Coord, LineString, MultiPolygon, Point, Polygon, coord};
use partition::{ZoneSets, partition};
use routers_tz_types::timezone::internal::{TimeZoneGeometry, TimeZoneName, TimezoneBuild};
use s2::cellid::CellID;
use s2::latlng::LatLng;

/// A zigzag border, an enclave in a hole, an antimeridian split, a polar border at 67.5°W with rings
/// of both windings (as in the source data), and an overlap.
fn zones() -> Vec<TimezoneBuild> {
    let ring = |points: &[(f64, f64)]| LineString::from(points.to_vec());
    let rect = |x0, y0, x1, y1| ring(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]);
    let clockwise = |ring: LineString| LineString(ring.0.into_iter().rev().collect());
    let zigzag = [
        (10.0, 0.0),
        (10.3, 2.5),
        (9.7, 5.0),
        (10.3, 7.5),
        (10.0, 10.0),
    ];
    let west = [&zigzag[..], &[(0.0, 10.0), (0.0, 0.0), (10.0, 0.0)]].concat();
    let east = [
        &[(20.0, 10.0)],
        &zigzag.iter().rev().copied().collect::<Vec<_>>()[..],
        &[(20.0, 0.0), (20.0, 10.0)],
    ]
    .concat();

    let zone = |name: &str, polygons: Vec<Polygon>| TimezoneBuild {
        name: TimeZoneName::new(name.to_string()),
        geometry: TimeZoneGeometry(MultiPolygon(polygons)),
    };
    vec![
        zone(
            "west",
            vec![Polygon::new(ring(&west), vec![rect(4.0, 4.0, 6.0, 6.0)])],
        ),
        zone("east", vec![Polygon::new(ring(&east), vec![])]),
        zone(
            "enclave",
            vec![Polygon::new(rect(4.0, 4.0, 6.0, 6.0), vec![])],
        ),
        zone(
            "dateline",
            vec![
                Polygon::new(rect(175.0, -20.0, 180.0, -10.0), vec![]),
                Polygon::new(rect(-180.0, -20.0, -175.0, -10.0), vec![]),
            ],
        ),
        zone(
            "pole_west",
            vec![Polygon::new(rect(-180.0, -90.0, -67.5, -60.0), vec![])],
        ),
        zone(
            "pole_east",
            vec![Polygon::new(
                clockwise(rect(-67.5, -90.0, 180.0, -60.0)),
                vec![],
            )],
        ),
        zone(
            "overlap",
            vec![Polygon::new(rect(15.0, 5.0, 25.0, 15.0), vec![])],
        ),
    ]
}

fn truth(zones: &[TimezoneBuild], at: Coord) -> Vec<u16> {
    (0..zones.len() as u16)
        .filter(|&zone| zones[zone as usize].geometry.0.contains(&Point(at)))
        .collect()
}

fn samples(zones: &[TimezoneBuild], per_zone: usize) -> Vec<Coord> {
    use geo::BoundingRect;
    let mut seed = 0x9E37_79B9_7F4A_7C15_u64;
    let mut unit = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as f64 / (1u64 << 53) as f64
    };

    (zones.iter())
        .flat_map(|zone| {
            let bounds = zone.geometry.0.bounding_rect().expect("zone has bounds");
            let (min, max) = (bounds.min() - coord! { x: 1.0, y: 1.0 }, bounds.max() + coord! { x: 1.0, y: 1.0 });
            (0..per_zone)
                .map(|_| coord! { x: min.x + unit() * (max.x - min.x), y: (min.y + unit() * (max.y - min.y)).clamp(-89.99, 89.99) })
                .collect::<Vec<_>>()
        })
        .collect()
}

fn assert_matches_truth(
    zones: &[TimezoneBuild],
    lookup: impl Fn(Coord) -> Option<Vec<u16>>,
    tolerance: f64,
) -> usize {
    let mut checked = 0;
    for at in samples(zones, 2000) {
        let expected = truth(zones, at);
        let dx = (tolerance / at.y.to_radians().cos()).min(360.0);
        let near_border = [
            (-dx, -tolerance),
            (-dx, tolerance),
            (dx, -tolerance),
            (dx, tolerance),
        ]
        .iter()
        .any(|&(x, y)| truth(zones, at + coord! { x: x, y: y }) != expected);
        if expected.is_empty() || near_border {
            continue;
        }

        checked += 1;
        assert_eq!(lookup(at), Some(expected), "at {at:?}");
    }
    checked
}

fn s2_lookup(cells: &[(CellID, Vec<u16>)], at: Coord) -> Option<Vec<u16>> {
    let leaf = CellID::from(&LatLng::from_degrees(at.y, at.x));
    (0..=s2_cells::MAX_LEVEL)
        .rev()
        .find_map(|level| {
            cells
                .binary_search_by_key(&leaf.parent(level), |(id, _)| *id)
                .ok()
        })
        .map(|pos| cells[pos].1.clone())
}

fn s2_partition() -> Vec<(CellID, Vec<u16>)> {
    let mut cells = partition(s2_cells::roots(), &zones());
    cells.sort_unstable_by_key(|(id, _)| *id);
    cells
}

#[test]
fn s2_cells_match_exact_containment() {
    let zones = zones();
    let cells = s2_partition();
    let checked = assert_matches_truth(&zones, |at| s2_lookup(&cells, at), 0.03);
    assert!(
        checked > 5000,
        "only {checked} samples were away from a border"
    );
}

#[test]
fn s2_cells_are_disjoint_with_sorted_zones() {
    let cells = s2_partition();
    assert!(
        cells
            .windows(2)
            .all(|w| w[0].0.range_max() < w[1].0.range_min())
    );
    assert!(
        (cells.iter()).all(|(id, zones)| id.level() <= s2_cells::MAX_LEVEL
            && !zones.is_empty()
            && zones.is_sorted())
    );
}

#[test]
fn s2_cells_hold_overlaps_enclaves_and_both_sides_of_the_antimeridian() {
    let cells = s2_partition();
    let at = |x, y| s2_lookup(&cells, coord! { x: x, y: y });
    assert_eq!(at(17.5, 7.5), Some(vec![1, 6]));
    assert_eq!(at(5.0, 5.0), Some(vec![2]));
    assert_eq!(at(177.5, -15.0), Some(vec![3]));
    assert_eq!(at(-177.5, -15.0), Some(vec![3]));
    assert_eq!(at(-90.0, -70.0), Some(vec![4]));
    assert_eq!(at(-45.0, -70.0), Some(vec![5]));
    assert_eq!(at(90.0, 45.0), None);
}

#[test]
fn zone_sets_number_overlaps_past_the_last_zone() {
    let mut sets = ZoneSets::new(7);
    assert_eq!(sets.index(vec![3]), 3);
    assert_eq!(sets.index(vec![1, 6]), 7);
    assert_eq!(sets.index(vec![2, 4, 5]), 8);
    assert_eq!(sets.index(vec![1, 6]), 7);
    assert_eq!(sets.overlaps, vec![vec![1, 6], vec![2, 4, 5]]);
}
